use std::collections::HashMap;
use std::path::Path;

use crate::context::AppContext;
use crate::grep_executor::{self, GrepParams};
use crate::pattern_compile::{self, CompileOpts, CompileResult};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{build_path_filters, GrepMatch, GrepResult, IndexStatus, WalkBound};

pub(crate) use crate::grep_executor::ripgrep_glob;

/// Matching lines the grep tool shows per page unless the caller asks for a
/// different page size through `max_results`.
const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_LINE_CHARS: usize = 200;
const MAX_MATCHES_PER_FILE: usize = 10;
const MAX_DISPLAY_MATCHES_PER_FILE: usize = 5;
/// Longest matching line, in characters, the grep tool prints before cutting
/// it and appending `GREP_LINE_TRUNCATED_MARKER`.
pub(crate) const GREP_MAX_LINE_CHARS: usize = 500;
/// Byte budget for the match rows of one grep reply (file headers included).
/// Rows past the budget are left for the next page instead of being printed.
pub(crate) const GREP_MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// Appended to a matching line cut at `GREP_MAX_LINE_CHARS`, so the reader
/// knows the printed text is not the whole line.
pub(crate) const GREP_LINE_TRUNCATED_MARKER: &str = "… [line truncated]";
/// Text note for a grep or glob whose search path holds no searchable file.
pub(crate) const NO_FILES_IN_SCOPE_NOTE: &str =
    "(No files were found after default directory skips (.git, node_modules, target, etc.); nothing was searched.)";

pub fn handle_grep(req: &RawRequest, ctx: &AppContext) -> Response {
    crate::bounded_io::command(req, ctx, handle_grep_inner)
}

fn handle_grep_inner(req: &RawRequest, ctx: &AppContext) -> Response {
    let pattern = match req.params.get("pattern").and_then(|value| value.as_str()) {
        Some(pattern) => pattern,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "grep: missing required param 'pattern'",
            );
        }
    };

    let case_sensitive = req
        .params
        .get("case_sensitive")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let include = string_array_param(&req.params, "include");
    let exclude = string_array_param(&req.params, "exclude");
    // `max_results` is the page size; `offset` skips that many matching lines
    // of the ordered result. The engine collects at most `offset + page_size`
    // matches, so the cap applies while matches are gathered rather than to a
    // full list afterwards.
    let page_size = req
        .params
        .get("max_results")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_RESULTS);
    let offset = req
        .params
        .get("offset")
        .and_then(|value| value.as_u64())
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
        .unwrap_or(0);
    let max_results = offset.saturating_add(page_size);

    let compiled = match pattern_compile::compile(
        pattern,
        CompileOpts {
            case_insensitive: !case_sensitive,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            return Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                serde_json::json!({"pattern": pattern}),
            );
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            return Response::error_with_data(
                &req.id,
                "invalid_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Use hint:'literal' or rewrite without {feature}."
                ),
                serde_json::json!({"pattern": pattern, "feature": feature}),
            );
        }
    };

    let filters = match build_path_filters(&include, &exclude) {
        Ok(filters) => filters,
        Err(error) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("grep: invalid include/exclude glob: {}", error),
            );
        }
    };

    let scope = match grep_executor::resolve_grep_scope(
        ctx,
        req.params.get("path"),
        max_results,
        &req.id,
        req.session(),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let project_root = grep_executor::project_root(ctx);
    let single_file_scope = scope.roots.len() == 1
        && crate::bounded_io::metadata(&scope.roots[0].search_root).is_ok_and(|m| m.is_file());
    let total_started = std::time::Instant::now();
    let search_start = std::time::Instant::now();
    let params = GrepParams {
        include,
        exclude,
        max_results,
        path_exclusion: None,
    };
    // A parent folder session answers from its child repositories' indexes.
    // Child indexes intentionally omit ignored targets, just like the local
    // index. An explicitly named ignored root must use the direct walk instead.
    let parent_answer = if crate::context::confinement::active()
        || scope.roots.iter().any(|root| root.ignored_target)
    {
        None
    } else {
        crate::views::parent::grep_fan_out(ctx, &compiled, &scope, &params, &filters)
    };
    let (result, phases, parent_gaps) = match parent_answer {
        Some(answer) => answer,
        None => {
            let (result, phases) = grep_executor::execute_profiled_with_filters(
                ctx, &compiled, &scope, &params, &filters,
            );
            (result, phases, Vec::new())
        }
    };
    let search_ms = search_start.elapsed().as_secs_f64() * 1000.0;
    let scope_probe_started = std::time::Instant::now();
    let scope_presence = phases
        .indexed_scope_has_files
        .or_else(|| {
            phases.walk_counts.and_then(|counts| {
                (!result.walk_truncated && result.skipped_foreign_mounts == 0)
                    .then_some(counts.eligible > 0)
            })
        })
        .or_else(|| grep_executor::scope_has_files(&scope, &filters));
    let scope_has_files = scope_presence != Some(false);
    let empty_counts = if !scope_has_files {
        grep_executor::scope_file_counts(&scope, &filters)
    } else {
        None
    };
    let scope_probe = scope_probe_started.elapsed();
    let format_started = std::time::Instant::now();
    let page_matches: Vec<&GrepMatch> =
        result.matches.iter().skip(offset).take(page_size).collect();
    let page = render_grep_page(&page_matches, &project_root, GREP_MAX_OUTPUT_BYTES);
    let next_offset = grep_next_offset(&result, &page, offset);
    let text = grep_page_text(&result, &page, offset, single_file_scope);
    let post_filter_format = phases.query.post_filter + format_started.elapsed();
    crate::slog_debug!(
        "perf grep phases: snapshot_acquire={:.3}ms query_decomposition={:.3}ms trigram_lookup={:.3}ms pread_verify={:.3}ms candidates={} bytes={} post_filter/format={:.3}ms scope_probe={:.3}ms total={:.3}ms",
        phases.snapshot_acquire.as_secs_f64() * 1000.0,
        phases.query_decomposition.as_secs_f64() * 1000.0,
        phases.query.trigram_lookup.as_secs_f64() * 1000.0,
        phases.query.pread_verify.as_secs_f64() * 1000.0,
        phases.query.candidate_count,
        phases.query.bytes_verified,
        post_filter_format.as_secs_f64() * 1000.0,
        scope_probe.as_secs_f64() * 1000.0,
        total_started.elapsed().as_secs_f64() * 1000.0,
    );

    let mut body = serde_json::json!({
        "text": text,
        "complete": scope_presence.is_some() && !result.walk_truncated
            && (scope_has_files || empty_counts.is_some())
            && !result.scan_deadline_reached
            && result.skipped_foreign_mounts == 0
            && result.missing_on_disk == 0,
        "no_files_matched_scope": !scope_has_files,
        "skipped_foreign_mounts": result.skipped_foreign_mounts,
        "matches": page_matches.iter().map(|m| match_to_json(m)).collect::<Vec<_>>(),
        "rendered_matches": page.rendered,
        "offset": offset,
        "output_byte_capped": page.byte_capped,
        "lines_truncated": page.lines_truncated,
        "next_offset": next_offset,
        "total_matches": result.total_matches,
        "files_searched": result.files_searched,
        "files_read_directly": result.files_read_directly,
        "files_with_matches": result.files_with_matches,
        "index_status": result.index_status.as_str(),
        "truncated": result.truncated,
        "search_ms": (search_ms * 1000.0).round() / 1000.0,
    });
    if let Some(bound) = result.walk_bound {
        body["walk_stopped_by"] = serde_json::Value::String(
            match bound {
                WalkBound::TimeBudget => "time_budget",
                WalkBound::FileLimit(_) => "file_limit",
                WalkBound::Cancelled => "cancelled",
            }
            .to_string(),
        );
    }
    if result.walk_truncated {
        body["walk_truncated"] = serde_json::Value::Bool(true);
        let note = walk_coverage_note(&result).unwrap_or_else(|| {
            "(Fallback directory walk stopped early: file-count or time budget reached; results may be incomplete.)".to_string()
        });
        body["text"] = serde_json::Value::String(format!("{}\n\n{}", text, note));
    }
    if result.scan_deadline_reached {
        // The scan ran out of time with candidate files still unread, so a
        // missing match here is not evidence of absence.
        body["scan_deadline_reached"] = serde_json::Value::Bool(true);
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(Search stopped at its {}-second time budget before every candidate file was searched; matches may be missing. Narrow with path or include.)",
            body["text"].as_str().unwrap_or_default(),
            crate::grep_executor::FALLBACK_WALK_BUDGET.as_secs()
        ));
    }
    if scope_presence.is_none() {
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(Scope probe reached its work bound; whether eligible files exist is unknown.)",
            body["text"].as_str().unwrap_or_default()
        ));
    }
    if !scope_has_files {
        // Say in the text itself that nothing was searched, so an empty scope
        // never reads as a searched directory with zero matches.
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n{}",
            body["text"].as_str().unwrap_or_default(),
            empty_counts.map(|counts| counts.empty_note(!params.include.is_empty() && params.exclude.is_empty()))
                .unwrap_or_else(|| "(0 files searched: the bounded walk could not determine why no files matched the scope; nothing was searched.)".to_string())
        ));
        if let Some(counts) = empty_counts {
            body["files_examined"] = serde_json::json!(counts.examined);
            body["ignored_items"] = serde_json::json!(counts.ignored);
            body["files_excluded_by_patterns"] = serde_json::json!(counts.filtered);
        }
    }
    if result.skipped_foreign_mounts > 0 {
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(Fallback directory walk skipped {} foreign filesystem mount(s); results may be incomplete.)",
            body["text"].as_str().unwrap_or_default(),
            result.skipped_foreign_mounts
        ));
    }
    if result.missing_on_disk > 0 {
        // The index listed files this checkout does not have, so it is out of
        // date and may also be missing content this checkout does have.
        body["missing_on_disk_dropped"] = serde_json::json!(result.missing_on_disk);
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n({} indexed file(s) were not on disk in this checkout; the index is out of date, so matches in files it has not seen may be missing.)",
            body["text"].as_str().unwrap_or_default(),
            result.missing_on_disk
        ));
    }

    // Disclose the trigram index state and whether this answer came from a
    // filesystem walk instead of the index. A missing or building index never
    // turns into "no matches": the walk above still searched every file.
    body["index"] = if crate::context::confinement::active() {
        serde_json::json!({"trigram": {"status":"fallback", "reason":"confined root walk"}})
    } else {
        trigram_index_disclosure(ctx)
    };
    body["fallback"] = if result.index_status == IndexStatus::Ready {
        serde_json::Value::Null
    } else {
        serde_json::Value::String("filesystem".to_string())
    };
    crate::views::parent::attach_gaps(&mut body, parent_gaps);
    crate::context::confinement::disclose_denied(&mut body);

    if let Some(envelope) = crate::list_surfaces::grep::build_grep_envelope(&body) {
        body["matches_list_envelope"] = serde_json::to_value(&envelope).unwrap_or_default();
    }

    let service_ms = total_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    match result.index_status {
        crate::search_index::IndexStatus::Ready => {
            let status =
                if result.truncated || result.walk_truncated || result.scan_deadline_reached {
                    "partial"
                } else {
                    "ok"
                };
            ctx.note_index_query(
                crate::logging::IndexPlane::Search,
                "grep",
                service_ms,
                status,
            );
        }
        crate::search_index::IndexStatus::Building => {
            ctx.note_index_query(
                crate::logging::IndexPlane::Search,
                "grep",
                service_ms,
                "building",
            );
        }
        crate::search_index::IndexStatus::Fallback | crate::search_index::IndexStatus::Disabled => {
        }
    }

    Response::success(&req.id, body)
}

/// `{"trigram": {status, reason}}` for grep/glob responses, from the shared
/// engine observation. Reading it never starts a build.
pub(crate) fn trigram_index_disclosure(ctx: &AppContext) -> serde_json::Value {
    serde_json::json!({
        "trigram": crate::feature_status::observed_index_status(
            ctx,
            crate::feature_status::IndexPlane::Trigram,
        )
        .consumer_json(),
    })
}

pub(crate) fn format_grep_text(result: &GrepResult, project_root: &Path) -> String {
    // Preserve the incoming match order (callers sort by mtime-desc upstream via
    // sort_grep_matches_by_mtime_desc). A BTreeMap here would re-sort groups
    // alphabetically by path and silently discard that ordering — both wasting
    // the upstream sort and giving a different ordering contract than the
    // semantic path, which groups in result order. Group by file preserving
    // first-appearance order instead.
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&GrepMatch>> = HashMap::new();

    for grep_match in &result.matches {
        // Use relative path within project, absolute otherwise
        let display_path = grep_match
            .file
            .strip_prefix(project_root)
            .unwrap_or(&grep_match.file)
            .display()
            .to_string();
        if !groups.contains_key(&display_path) {
            group_order.push(display_path.clone());
        }
        groups.entry(display_path).or_default().push(grep_match);
    }

    let mut sections = Vec::new();

    for file in &group_order {
        let matches = &groups[file];
        let mut section = file.clone();
        let display_count = if matches.len() > MAX_MATCHES_PER_FILE {
            MAX_DISPLAY_MATCHES_PER_FILE
        } else {
            matches.len()
        };

        for grep_match in matches.iter().take(display_count) {
            section.push_str(&format!(
                "\n{}: {}",
                grep_match.line,
                truncate_line_text(&grep_match.line_text)
            ));
        }

        if matches.len() > MAX_MATCHES_PER_FILE {
            section.push_str(&format!(
                "\n... and {} more matches",
                matches.len() - MAX_DISPLAY_MATCHES_PER_FILE
            ));
        }

        sections.push(section);
    }

    let footer = grep_footer(result);

    if sections.is_empty() {
        footer
    } else {
        format!("{}\n\n{}", sections.join("\n\n"), footer)
    }
}

fn grep_footer(result: &GrepResult) -> String {
    // Wholesale-singular ("40 match across 4 file") — the `(es)`/`(s)` plural
    // parentheticals were pure per-call token tax for zero agent value. The
    // `[index: ready]` tag is dropped on the common ready path (absence == ready,
    // mirroring the aft_search precedent); non-ready states keep a label because
    // "building"/"fallback"/"disabled" is a real completeness signal the agent
    // needs (results may be partial).
    // When the search stopped at the result cap, the count is a floor, not the
    // true total — say so in the agent-facing text (the JSON already carries
    // `truncated`). Otherwise the agent reads "Found N match" as exhaustive.
    let cap_note = if result.truncated { " (capped)" } else { "" };
    match result.index_status {
        IndexStatus::Ready => format!(
            "Found {} match across {} file{}",
            result.total_matches, result.files_with_matches, cap_note
        ),
        other => format!(
            "Found {} match across {} file{} [index: {}]",
            result.total_matches,
            result.files_with_matches,
            cap_note,
            index_status_label(other)
        ),
    }
}

/// The footer the grep tool prints. An index answer gets the plain footer.
/// Any other answer came from reading files directly (a fallback walk or a
/// named file), so its index label also says how much of the scope that read
/// covered: a zero next to "[index: building]" alone could mean either "no
/// match anywhere" or "the walk never got there", and the reader cannot tell
/// which without re-checking.
fn grep_tool_footer(result: &GrepResult) -> String {
    if result.index_status == IndexStatus::Ready {
        return grep_footer(result);
    }
    let cap_note = if result.truncated { " (capped)" } else { "" };
    format!(
        "Found {} match across {} file{} [index: {}; {}]",
        result.total_matches,
        result.files_with_matches,
        cap_note,
        index_status_label(result.index_status),
        direct_read_coverage(result)
    )
}

/// How much of the scope a direct (non-index) read covered, for the footer.
fn direct_read_coverage(result: &GrepResult) -> String {
    let files = result.files_read_directly;
    if let Some(bound) = result.walk_bound {
        return format!(
            "incomplete: checked {files} files before {}",
            walk_bound_phrase(bound)
        );
    }
    if result.scan_deadline_reached {
        return format!(
            "incomplete: stopped at the {}-second time budget",
            crate::grep_executor::FALLBACK_WALK_BUDGET.as_secs()
        );
    }
    if result.truncated || result.engine_capped {
        // The search stopped collecting once it had enough matches, so some
        // files were never read; the "(capped)" count already says the total
        // is a floor.
        return format!("stopped at the match limit after searching {files} files directly");
    }
    format!("searched all {files} files directly")
}

fn walk_bound_phrase(bound: WalkBound) -> String {
    match bound {
        WalkBound::TimeBudget => "the walk's time budget ran out".to_string(),
        WalkBound::FileLimit(limit) => format!("the walk's {limit}-file limit"),
        WalkBound::Cancelled => "the request was cancelled".to_string(),
    }
}

/// A note spelling out what an incomplete fallback walk left unsearched and
/// how to get an exhaustive answer, or `None` when the walk reached every
/// file (or no walk ran).
pub(crate) fn walk_coverage_note(result: &GrepResult) -> Option<String> {
    let bound = result.walk_bound?;
    let files = result.files_read_directly;
    let index = match result.index_status {
        IndexStatus::Ready => "index ready",
        IndexStatus::Building => "index building",
        IndexStatus::Fallback => "index not used",
        IndexStatus::Disabled => "index disabled",
    };
    let exhaustive = if result.index_status == IndexStatus::Building {
        "For an exhaustive search, narrow with path or include so the walk reaches every file, or retry once the index is ready (an index search reads every indexed file)."
    } else {
        "For an exhaustive search, narrow with path or include so the walk reaches every file."
    };
    let checked = match bound {
        WalkBound::FileLimit(limit) => format!(
            "checked {files} of more than {limit} files ({index}); the fallback walk stops at {limit} files, so files past that limit were not searched and matches may be missing."
        ),
        WalkBound::TimeBudget => format!(
            "checked {files} files ({index}); the fallback walk ran out of its time budget before reaching every file, so files it did not reach were not searched and matches may be missing."
        ),
        WalkBound::Cancelled => format!(
            "checked {files} files ({index}); the request was cancelled before the fallback walk reached every file, so matches may be missing."
        ),
    };
    Some(format!("({checked} {exhaustive})"))
}

/// One rendered page of grep output: the match rows that fit the byte budget,
/// grouped under their file path.
#[derive(Debug)]
pub(crate) struct GrepPage {
    /// File headers and match rows, without the footer.
    pub rows_text: String,
    /// Number of page matches printed; the rest of the page did not fit.
    pub rendered: usize,
    /// Number of page matches the page held before the byte budget applied.
    pub page_len: usize,
    /// Printed lines cut at `GREP_MAX_LINE_CHARS`.
    pub lines_truncated: usize,
    /// True when the byte budget stopped rendering before the page ended.
    pub byte_capped: bool,
}

/// Render one page of matches in result order, stopping before the first row
/// that would push the text past `max_bytes`. Every match is printed on its
/// own row; a file header is printed whenever the file changes, so the rows
/// stay in exactly the order used for `offset` paging.
pub(crate) fn render_grep_page(
    page: &[&GrepMatch],
    project_root: &Path,
    max_bytes: usize,
) -> GrepPage {
    let mut rows_text = String::new();
    let mut rendered = 0usize;
    let mut lines_truncated = 0usize;
    let mut byte_capped = false;
    let mut current_file: Option<&Path> = None;

    for grep_match in page {
        let (line_text, cut) = truncate_grep_line(&grep_match.line_text);
        let row = format!("{}: {}", grep_match.line, line_text);
        let new_file = current_file != Some(grep_match.file.as_path());
        let mut chunk = String::new();
        if new_file {
            if !rows_text.is_empty() {
                chunk.push_str("\n\n");
            }
            chunk.push_str(
                &grep_match
                    .file
                    .strip_prefix(project_root)
                    .unwrap_or(&grep_match.file)
                    .display()
                    .to_string(),
            );
        }
        chunk.push('\n');
        chunk.push_str(&row);

        // Always print at least one row so a page never comes back empty
        // while matches remain; a single row is bounded by the line cap.
        if rendered > 0 && rows_text.len() + chunk.len() > max_bytes {
            byte_capped = true;
            break;
        }
        rows_text.push_str(&chunk);
        rendered += 1;
        if cut {
            lines_truncated += 1;
        }
        current_file = Some(grep_match.file.as_path());
    }

    GrepPage {
        rows_text,
        rendered,
        page_len: page.len(),
        lines_truncated,
        byte_capped,
    }
}

/// Assemble the grep reply text: the page rows, the "Found N match" footer,
/// then one-line notes telling the reader how to get the rest.
pub(crate) fn grep_page_text(
    result: &GrepResult,
    page: &GrepPage,
    offset: usize,
    single_file_scope: bool,
) -> String {
    let footer = grep_tool_footer(result);
    let mut text = if page.rows_text.is_empty() {
        footer
    } else {
        format!("{}\n\n{}", page.rows_text, footer)
    };

    let next_offset = grep_next_offset(result, page, offset);
    if page.byte_capped {
        text.push_str(&format!(
            "\n\n(Output reached the {GREP_MAX_OUTPUT_BYTES}-byte limit after {} of {} rows on this page; continue with offset={}.)",
            page.rendered,
            page.page_len,
            offset.saturating_add(page.rendered)
        ));
    } else if let Some(next) = next_offset {
        text.push_str(&format!("\n\n(More matches; continue with offset={next}.)"));
    } else if offset > 0 && page.page_len == 0 {
        text.push_str(&format!(
            "\n\n(offset={offset} is past the last match; nothing left to show.)"
        ));
    }

    // The engine keeps the first matches it reaches and only then sorts them,
    // so when a search over several files stops at the collection bound, a
    // later page (collected with a larger bound) is not guaranteed to line up
    // with an earlier one. A single file is scanned in line order, and a
    // search that saw every match is sorted as a whole, so both page exactly.
    if result.truncated
        && !single_file_scope
        && (result.files_with_matches > 1 || result.engine_capped)
    {
        text.push_str(
            "\n\n(This search stopped at the match limit across several files, so pages fetched with offset may overlap or skip rows; narrow with path or include for exact paging.)",
        );
    }
    text
}

/// Offset of the first matching line not shown yet, when more remain.
pub(crate) fn grep_next_offset(
    result: &GrepResult,
    page: &GrepPage,
    offset: usize,
) -> Option<usize> {
    let next = offset.saturating_add(page.rendered);
    let more = page.rendered < page.page_len
        || result.truncated
        || result.total_matches > next
        || result.matches.len() > next;
    (more && page.page_len > 0).then_some(next)
}

/// Cut a matching line at `GREP_MAX_LINE_CHARS` characters and mark the cut.
/// Returns the printable text and whether it was cut.
pub(crate) fn truncate_grep_line(text: &str) -> (String, bool) {
    match text.char_indices().nth(GREP_MAX_LINE_CHARS) {
        None => (text.to_string(), false),
        Some((byte_index, _)) => (
            format!("{}{}", &text[..byte_index], GREP_LINE_TRUNCATED_MARKER),
            true,
        ),
    }
}

pub(crate) fn truncate_line_text(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= MAX_LINE_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX_LINE_CHARS).collect();
    format!("{}…", truncated)
}

fn index_status_label(status: IndexStatus) -> &'static str {
    match status {
        IndexStatus::Ready => "ready",
        IndexStatus::Building => "building",
        IndexStatus::Fallback => "fallback",
        IndexStatus::Disabled => "disabled",
    }
}

fn match_to_json(grep_match: &GrepMatch) -> serde_json::Value {
    serde_json::json!({
        "file": grep_match.file.display().to_string(),
        "line": grep_match.line,
        "column": grep_match.column,
        "line_text": grep_match.line_text,
        "match_text": grep_match.match_text,
    })
}

fn string_array_param(params: &serde_json::Value, key: &str) -> Vec<String> {
    let Some(value) = params.get(key) else {
        return Vec::new();
    };
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .flat_map(|raw| split_brace_aware(&raw))
            .filter(|item| !item.is_empty())
            .collect();
    }
    if let Some(raw) = value.as_str() {
        return split_brace_aware(raw)
            .into_iter()
            .filter(|item| !item.is_empty())
            .collect();
    }
    Vec::new()
}

/// Split a comma-separated glob string into multiple globs while preserving
/// brace alternations (`**/*.{ts,tsx}`). Treats `,` as a separator only when
/// the surrounding `{` / `}` depth is zero, mirroring the plugin-layer
/// `splitIncludeArg` so direct binary callers (bash rewrite, CLI users,
/// future hosts) get the same robustness.
///
/// Defends against issue #33: agents passing `"**/*.{ts,tsx},**/*.{js,jsx}"`
/// would previously be naively split by some caller into the two broken
/// fragments `**/*.{ts` and `**/*.{js`, both rejected by the globset parser.
/// This helper is brace-aware so the brace groups stay intact.
fn split_brace_aware(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    for ch in raw.chars() {
        match ch {
            '{' => {
                depth += 1;
                buf.push(ch);
            }
            '}' => {
                if depth > 0 {
                    depth -= 1;
                }
                buf.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = buf.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    let tail = buf.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn grep_match(file: &str, line: u32, line_text: &str) -> GrepMatch {
        GrepMatch {
            file: PathBuf::from(file),
            line,
            column: 1,
            line_text: line_text.to_string(),
            match_text: "needle".to_string(),
        }
    }

    fn grep_result(
        matches: Vec<GrepMatch>,
        total_matches: usize,
        files_searched: usize,
        files_with_matches: usize,
        index_status: IndexStatus,
        truncated: bool,
    ) -> GrepResult {
        GrepResult {
            matches,
            total_matches,
            files_searched,
            files_with_matches,
            index_status,
            truncated,
            fully_degraded: false,
            engine_capped: false,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: 0,
            scan_deadline_reached: false,
            files_read_directly: 0,
            walk_bound: None,
        }
    }

    fn root() -> PathBuf {
        PathBuf::from("/project")
    }

    #[test]
    fn truncate_at_char_boundary_at_40_bytes_mid_em_dash() {
        let mut line = "x".repeat(38);
        line.push('—');
        line.push('z');
        assert_eq!(line.len(), 42);
        assert!(!line.is_char_boundary(40));
        let safe = crate::grep_executor::truncate_at_char_boundary(&line, 40);
        assert!(safe.is_char_boundary(safe.len()));
        assert_eq!(safe.len(), 38);
    }

    #[test]
    fn grep_groups_truncates_and_adds_footer() {
        let long_line = format!("{}xyz", "a".repeat(220));
        let result = grep_result(
            vec![
                grep_match(
                    "/project/crates/aft/src/commands/grep.rs",
                    14,
                    "pub fn handle_grep(req: &RawRequest, ctx: &AppContext) -> Response {",
                ),
                grep_match("/project/crates/aft/src/commands/grep.rs", 116, &long_line),
                grep_match(
                    "/project/crates/aft/src/main.rs",
                    116,
                    "        \"grep\" => aft::commands::grep::handle_grep(&req, ctx),",
                ),
            ],
            3,
            2,
            2,
            IndexStatus::Ready,
            false,
        );

        let text = format_grep_text(&result, &root());

        // Relative paths, no decorators, no match count in header
        assert!(text.contains("crates/aft/src/commands/grep.rs\n"));
        assert!(text
            .contains("14: pub fn handle_grep(req: &RawRequest, ctx: &AppContext) -> Response {"));
        // Long line truncated at 200 chars
        assert!(text.contains("116: aaaaaaa"));
        assert!(text.contains("…"));
        assert!(text.contains("crates/aft/src/main.rs\n"));
        // Ready path drops the [index] tag (absence == ready); wholesale-singular.
        assert!(text.ends_with("Found 3 match across 2 file"));
    }

    #[test]
    fn grep_caps_large_file_sections() {
        let matches = (1..=11)
            .map(|line| grep_match("/project/src/large.rs", line, &format!("line {line}")))
            .collect::<Vec<_>>();
        let result = grep_result(matches, 11, 1, 1, IndexStatus::Fallback, false);

        let text = format_grep_text(&result, &root());

        assert!(text.contains("src/large.rs\n"));
        assert!(text.contains("1: line 1"));
        assert!(text.contains("5: line 5"));
        assert!(!text.contains("6: line 6"));
        assert!(text.contains("... and 6 more matches"));
    }

    #[test]
    fn grep_line_cut_counts_characters_not_bytes() {
        let line = "é".repeat(GREP_MAX_LINE_CHARS + 100);
        let (printed, cut) = truncate_grep_line(&line);
        assert!(cut);
        assert_eq!(
            printed,
            format!(
                "{}{}",
                "é".repeat(GREP_MAX_LINE_CHARS),
                GREP_LINE_TRUNCATED_MARKER
            )
        );

        let exact = "é".repeat(GREP_MAX_LINE_CHARS);
        assert_eq!(truncate_grep_line(&exact), (exact.clone(), false));
    }

    #[test]
    fn grep_returns_zero_results_footer() {
        let result = grep_result(Vec::new(), 0, 0, 0, IndexStatus::Fallback, false);

        let text = format_grep_text(&result, &root());

        // Non-ready (fallback) keeps the index label as a completeness signal;
        // wholesale-singular phrasing.
        assert_eq!(text, "Found 0 match across 0 file [index: fallback]");
    }

    // Issue #33 regression: brace-aware include/exclude splitting at the Rust
    // boundary. Defends direct binary callers (bash rewrite, CLI users) and
    // hosts that pass a comma-separated string instead of an already-split
    // array of globs.
    #[test]
    fn split_preserves_single_brace_group() {
        assert_eq!(split_brace_aware("**/*.{ts,tsx}"), vec!["**/*.{ts,tsx}"]);
    }

    #[test]
    fn split_handles_top_level_commas_with_braces() {
        assert_eq!(
            split_brace_aware("**/*.{ts,tsx},**/*.{js,jsx}"),
            vec!["**/*.{ts,tsx}", "**/*.{js,jsx}"],
        );
    }

    #[test]
    fn split_strips_whitespace_around_top_level_separators() {
        assert_eq!(
            split_brace_aware("**/*.{ts,tsx}, **/*.{js,jsx}"),
            vec!["**/*.{ts,tsx}", "**/*.{js,jsx}"],
        );
    }

    #[test]
    fn split_handles_nested_braces() {
        assert_eq!(
            split_brace_aware("**/{a,{b,c},d}.ts"),
            vec!["**/{a,{b,c},d}.ts"],
        );
    }

    #[test]
    fn split_tolerates_unbalanced_brace_without_panic() {
        // Don't crash on malformed input; treat the unclosed brace as part
        // of the buffer and let the globset parser surface the real error.
        let result = split_brace_aware("**/*.{ts,tsx");
        assert_eq!(result, vec!["**/*.{ts,tsx"]);
    }

    #[test]
    fn split_returns_empty_for_blank_input() {
        assert!(split_brace_aware("").is_empty());
        assert!(split_brace_aware("   ").is_empty());
    }

    #[test]
    fn string_array_param_accepts_string_with_braces() {
        let params = serde_json::json!({"include": "**/*.{ts,tsx},**/*.{js,jsx}"});
        let result = string_array_param(&params, "include");
        assert_eq!(result, vec!["**/*.{ts,tsx}", "**/*.{js,jsx}"]);
    }

    #[test]
    fn string_array_param_accepts_array_input() {
        let params = serde_json::json!({"include": ["**/*.ts", "**/*.tsx"]});
        let result = string_array_param(&params, "include");
        assert_eq!(result, vec!["**/*.ts", "**/*.tsx"]);
    }

    #[test]
    fn string_array_param_normalizes_array_with_brace_strings() {
        let params = serde_json::json!({"include": ["**/*.{ts,tsx}", "*.json"]});
        let result = string_array_param(&params, "include");
        assert_eq!(result, vec!["**/*.{ts,tsx}", "*.json"]);
    }

    fn compiled_literal(pattern: &str) -> crate::pattern_compile::CompiledPattern {
        match crate::pattern_compile::compile(
            pattern,
            crate::pattern_compile::CompileOpts {
                literal: true,
                ..crate::pattern_compile::CompileOpts::default()
            },
        ) {
            crate::pattern_compile::CompileResult::Ok(compiled) => compiled,
            other => panic!("compile literal {pattern:?}: {other:?}"),
        }
    }

    fn grep_result_bytes(result: &GrepResult, project_root: &Path) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "text": format_grep_text(result, project_root),
            "matches": result.matches.iter().map(match_to_json).collect::<Vec<_>>(),
            "total_matches": result.total_matches,
            "files_searched": result.files_searched,
            "files_with_matches": result.files_with_matches,
            "index_status": result.index_status.as_str(),
            "truncated": result.truncated,
            "fully_degraded": result.fully_degraded,
            "engine_capped": result.engine_capped,
            "walk_truncated": result.walk_truncated,
            "skipped_foreign_mounts": result.skipped_foreign_mounts,
        }))
        .expect("serialize grep result projection")
    }

    #[test]
    fn precompiled_path_filters_preserve_indexed_grep_bytes() {
        let project = tempfile::tempdir().expect("tempdir");
        let files = [
            ("src/lib.rs", "pub fn needle_one() {}\n"),
            ("src/lib.ts", "export const needle_two = 2;\n"),
            ("src/ignored/generated.rs", "fn needle_ignored() {}\n"),
            ("tests/lib.rs", "#[test] fn needle_test() {}\n"),
            ("README.md", "needle docs\n"),
        ];
        for (relative, content) in files {
            let path = project.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("fixture parent"))
                .expect("create fixture directory");
            std::fs::write(path, content).expect("write fixture");
        }

        let index = crate::search_index::SearchIndex::build(project.path());
        let snapshot = index.snapshot();
        let cases = [
            (vec!["**/*.rs".to_string()], Vec::new(), 100),
            (
                vec!["**/*.{rs,ts}".to_string()],
                vec!["**/ignored/**".to_string()],
                100,
            ),
            (Vec::new(), vec!["tests/**".to_string()], 2),
            (vec!["**/*.md".to_string()], Vec::new(), 100),
        ];
        let pattern = compiled_literal("needle");

        for (include, exclude, max_results) in cases {
            let legacy =
                snapshot.search_grep(&pattern, &include, &exclude, project.path(), max_results);
            let filters = build_path_filters(&include, &exclude).expect("valid filters");
            let optimized = snapshot
                .search_grep_profiled_with_filters(
                    &pattern,
                    &filters,
                    project.path(),
                    max_results,
                    None,
                )
                .0;
            assert_eq!(
                grep_result_bytes(&optimized, project.path()),
                grep_result_bytes(&legacy, project.path()),
                "include={include:?} exclude={exclude:?} max_results={max_results}",
            );
        }
    }

    /// Manual release-mode probe for the request setup paid by indexed grep.
    ///
    /// The fixture models a warm 12k-file TypeScript repository and a no-hit
    /// query, a common interactive shape where index lookup is deliberately
    /// cheap and fixed per-request setup is a material share of latency.
    #[test]
    #[ignore = "manual release-mode indexed-grep performance probe"]
    fn grep_path_filter_reuse_perf_probe() {
        const FILES: usize = 12_000;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 500;

        let project_root = PathBuf::from("/tmp/aft-grep-filter-perf-project");
        let mut index = crate::search_index::SearchIndex::new();
        for file_index in 0..FILES {
            index.index_file(
                &project_root.join(format!("src/pkg_{file_index:05}/lib.ts")),
                b"export const indexed_value = 'warm corpus';\n",
            );
        }
        let snapshot = index.snapshot();
        let pattern = compiled_literal("definitely_absent_grep_token_47");
        let include = vec!["**/*.{ts,tsx}".to_string()];
        let exclude = Vec::new();

        let legacy_once = || {
            let validated = build_path_filters(&include, &exclude).expect("valid filters");
            std::hint::black_box(&validated);
            let result = snapshot.search_grep(&pattern, &include, &exclude, &project_root, 100);
            std::hint::black_box(result.total_matches);
        };
        let optimized_once = || {
            let filters = build_path_filters(&include, &exclude).expect("valid filters");
            let result = snapshot
                .search_grep_profiled_with_filters(&pattern, &filters, &project_root, 100, None)
                .0;
            std::hint::black_box(result.total_matches);
        };

        let mut legacy_ns = Vec::with_capacity(SAMPLES);
        let mut optimized_ns = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let measure = |operation: &dyn Fn()| {
                let started = std::time::Instant::now();
                for _ in 0..ITERATIONS {
                    operation();
                }
                started.elapsed().as_nanos() / ITERATIONS as u128
            };
            if sample % 2 == 0 {
                legacy_ns.push(measure(&legacy_once));
                optimized_ns.push(measure(&optimized_once));
            } else {
                optimized_ns.push(measure(&optimized_once));
                legacy_ns.push(measure(&legacy_once));
            }
        }
        legacy_ns.sort_unstable();
        optimized_ns.sort_unstable();
        let legacy_median = legacy_ns[SAMPLES / 2];
        let optimized_median = optimized_ns[SAMPLES / 2];
        let speedup = legacy_median as f64 / optimized_median as f64;
        let saved_percent =
            100.0 * (legacy_median.saturating_sub(optimized_median)) as f64 / legacy_median as f64;

        eprintln!(
            "grep path-filter setup: files={FILES} samples={SAMPLES} iterations={ITERATIONS}"
        );
        eprintln!("legacy ns/op samples: {legacy_ns:?}");
        eprintln!("reused ns/op samples: {optimized_ns:?}");
        eprintln!(
            "median: legacy={legacy_median}ns reused={optimized_median}ns speedup={speedup:.2}x saved={saved_percent:.1}%"
        );
    }
}
