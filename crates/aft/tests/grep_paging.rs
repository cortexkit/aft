//! Paging, output byte budget and long-line handling of the grep tool.
//!
//! The grep tool prints up to 100 matching lines per call, cuts lines longer
//! than 500 characters, stops a page once its rows reach 50 KB, and takes an
//! `offset` to fetch the rest. The limits are written out as literals here so
//! a change to any of them is a visible contract change.

use std::fs;
use std::path::{Path, PathBuf};

use aft::subc_format::{format_response_with_context, FormatContext};
use serde_json::Value;

/// Byte budget for the match rows of one grep reply.
const OUTPUT_BYTES: usize = 51_200;
/// Characters of a matching line printed before it is cut.
const LINE_CHARS: usize = 500;
/// Marker appended to a cut line.
const LINE_MARKER: &str = "… [line truncated]";

fn grep_ctx(project: &Path) -> aft::context::AppContext {
    let config = aft::config::Config {
        project_root: Some(project.to_path_buf()),
        ..aft::config::Config::default()
    };
    aft::context::AppContext::from_app(
        aft::context::App::default_shared(),
        crate::context_storage::isolate(config),
    )
}

/// Run grep through the command handler and the agent-facing formatter,
/// returning the JSON payload and the text the agent reads.
fn run_grep(ctx: &aft::context::AppContext, params: Value) -> (Value, String) {
    let req = aft::protocol::RawRequest {
        id: "grep-page".into(),
        command: "grep".into(),
        lsp_hints: None,
        session_id: None,
        params,
    };
    let response = aft::commands::grep::handle_grep(&req, ctx);
    assert!(response.success, "grep failed: {:?}", response.data);
    let formatted = format_response_with_context("grep", &response, &FormatContext::default());
    (response.data, formatted)
}

/// Line numbers of the match rows printed in grep text (`<line>: <text>`).
fn printed_lines(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| {
            line.split_once(": ")
                .and_then(|(n, _)| n.parse::<u32>().ok())
        })
        .collect()
}

fn write_needle_file(project: &Path, name: &str, lines: usize, filler: &str) -> PathBuf {
    let path = project.join(name);
    let mut content = String::new();
    for line in 1..=lines {
        content.push_str(&format!("needle {filler} row {line}\n"));
    }
    fs::write(&path, content).expect("write fixture");
    path
}

#[test]
fn grep_single_file_150_matches_pages_by_offset() {
    let project = tempfile::tempdir().expect("tempdir");
    let file = write_needle_file(project.path(), "many.txt", 150, "x");
    let ctx = grep_ctx(project.path());
    let path = file.to_string_lossy().to_string();

    // Page 1: the first 100 matching lines, and a trailer naming offset.
    let (data, text) = run_grep(&ctx, serde_json::json!({"pattern": "needle", "path": path}));
    let page_one = printed_lines(&text);
    assert_eq!(
        page_one,
        (1..=100).collect::<Vec<u32>>(),
        "page 1 text:\n{text}"
    );
    assert_eq!(data["rendered_matches"], 100);
    assert_eq!(data["next_offset"], 100);
    assert!(text.contains("(More matches; continue with offset=100.)"));
    assert!(
        text.ends_with("shown 100 of ≥101 rows (cap) · narrow: offset, path, include, exclude"),
        "page 1 text:\n{text}"
    );
    // A single file is scanned in line order, so paging is exact and the
    // several-files warning must not appear.
    assert!(!text.contains("may overlap or skip rows"));

    // Page 2: offset=100 returns the remaining 50, contiguous with page 1.
    let (data, text) = run_grep(
        &ctx,
        serde_json::json!({"pattern": "needle", "path": path, "offset": 100}),
    );
    let page_two = printed_lines(&text);
    assert_eq!(
        page_two,
        (101..=150).collect::<Vec<u32>>(),
        "page 2 text:\n{text}"
    );
    assert_eq!(data["rendered_matches"], 50);
    assert!(data["next_offset"].is_null());
    assert!(!text.contains("continue with offset"));
    assert!(
        text.ends_with("shown 50 of 150 rows (cap) · narrow: offset, path, include, exclude"),
        "page 2 text:\n{text}"
    );
}

#[test]
fn grep_pages_are_disjoint_contiguous_and_in_the_same_order() {
    let project = tempfile::tempdir().expect("tempdir");
    let file = write_needle_file(project.path(), "ordered.txt", 250, "y");
    let ctx = grep_ctx(project.path());
    let path = file.to_string_lossy().to_string();

    // One page large enough to hold every match is the reference order.
    let (_, whole) = run_grep(
        &ctx,
        serde_json::json!({"pattern": "needle", "path": path, "max_results": 1000}),
    );
    let reference = printed_lines(&whole);
    assert_eq!(reference.len(), 250);

    let mut paged = Vec::new();
    for offset in [0, 100, 200] {
        let (_, text) = run_grep(
            &ctx,
            serde_json::json!({"pattern": "needle", "path": path, "offset": offset}),
        );
        paged.extend(printed_lines(&text));
    }
    assert_eq!(paged, reference);

    // The same page asked twice comes back identical.
    let again = |offset: u64| {
        run_grep(
            &ctx,
            serde_json::json!({"pattern": "needle", "path": path, "offset": offset}),
        )
        .1
    };
    assert_eq!(again(100), again(100));
}

#[test]
fn grep_output_byte_cap_stops_the_page_and_is_disclosed() {
    let project = tempfile::tempdir().expect("tempdir");
    // Each row carries 450 two-byte characters (~900 bytes), so the 50 KB
    // budget fills well before 100 rows.
    let filler = "é".repeat(450);
    let file = write_needle_file(project.path(), "wide.txt", 150, &filler);
    let ctx = grep_ctx(project.path());
    let path = file.to_string_lossy().to_string();

    let (data, text) = run_grep(&ctx, serde_json::json!({"pattern": "needle", "path": path}));
    let rendered = data["rendered_matches"].as_u64().expect("rendered_matches") as usize;
    assert!(rendered > 0 && rendered < 100, "rendered {rendered}");
    assert_eq!(data["output_byte_capped"], true);
    assert_eq!(data["matches"].as_array().expect("matches").len(), 100);
    assert_eq!(printed_lines(&text).len(), rendered);
    assert!(
        text.len() < OUTPUT_BYTES + 1024,
        "text is {} bytes",
        text.len()
    );
    assert!(text.contains(&format!(
        "(Output reached the {}-byte limit after {rendered} of 100 rows on this page; continue with offset={rendered}.)",
        OUTPUT_BYTES
    )));
    assert!(text.ends_with(&format!(
        "shown {rendered} of ≥101 rows (cap) · narrow: offset, path, include, exclude"
    )));

    // The next page starts right after the last printed row.
    let (_, next) = run_grep(
        &ctx,
        serde_json::json!({"pattern": "needle", "path": path, "offset": rendered}),
    );
    assert_eq!(
        printed_lines(&next).first().copied(),
        Some(rendered as u32 + 1)
    );
}

#[test]
fn grep_long_line_is_cut_and_marked() {
    let project = tempfile::tempdir().expect("tempdir");
    let file = project.path().join("long.txt");
    let long_line = format!("needle {}", "z".repeat(800));
    fs::write(&file, format!("{long_line}\nneedle short\n")).expect("write fixture");
    let ctx = grep_ctx(project.path());

    let (data, text) = run_grep(
        &ctx,
        serde_json::json!({"pattern": "needle", "path": file.to_string_lossy()}),
    );
    assert_eq!(data["lines_truncated"], 1);
    let printed = format!("1: {}{}", &long_line[..LINE_CHARS], LINE_MARKER);
    assert!(text.contains(&format!("{printed}\n")), "text:\n{text}");
    assert!(text.contains("2: needle short"));
    // The JSON carries the same cut as the printed row. Keeping a whole
    // multi-megabyte minified line per match is what made grep quadratic on
    // such files, and every renderer prints at most this prefix anyway.
    assert_eq!(
        data["matches"][0]["line_text"],
        format!("{}{}", &long_line[..LINE_CHARS], LINE_MARKER)
    );
}

#[test]
fn grep_capped_search_across_files_discloses_inexact_paging() {
    let project = tempfile::tempdir().expect("tempdir");
    for i in 0..3 {
        write_needle_file(project.path(), &format!("part_{i}.txt"), 60, "w");
    }
    let ctx = grep_ctx(project.path());

    let (data, text) = run_grep(&ctx, serde_json::json!({"pattern": "needle"}));
    assert_eq!(data["truncated"], true);
    assert_eq!(data["rendered_matches"], 100);
    assert!(
        text.contains("pages fetched with offset may overlap or skip rows; narrow with path or include for exact paging."),
        "text:\n{text}"
    );
    assert!(text.ends_with("narrow: offset, path, include, exclude"));

    // Offset 100 makes the engine collect up to 200 matches (offset plus page
    // size), which covers all 180, so the search is complete and the warning
    // goes away.
    let (data, text) = run_grep(
        &ctx,
        serde_json::json!({"pattern": "needle", "offset": 100}),
    );
    assert_eq!(data["truncated"], false);
    assert_eq!(data["rendered_matches"], 80);
    assert!(!text.contains("may overlap or skip rows"), "text:\n{text}");
    assert!(text.ends_with("shown 80 of 180 rows (cap) · narrow: offset, path, include, exclude"));
}

#[test]
fn grep_offset_past_the_end_says_so() {
    let project = tempfile::tempdir().expect("tempdir");
    let file = write_needle_file(project.path(), "few.txt", 3, "v");
    let ctx = grep_ctx(project.path());

    let (data, text) = run_grep(
        &ctx,
        serde_json::json!({"pattern": "needle", "path": file.to_string_lossy(), "offset": 10}),
    );
    assert_eq!(data["rendered_matches"], 0);
    assert!(text.contains("(offset=10 is past the last match; nothing left to show.)"));
}
