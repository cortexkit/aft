use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::file_kind::{ContentKind, FileKind, FileKindCache};
use crate::grep_executor::{self, GrepParams};
use crate::pattern_compile::{self, CompileOpts, CompileResult};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, GrepMatch, GrepResult, GrepScanOrder, IndexStatus, WalkBound,
};

pub(crate) use crate::grep_executor::ripgrep_glob;

/// Matching lines the grep tool shows on a page after the first unless the
/// caller asks for a different page size through `max_results`.
const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_GREP_OFFSET: usize = 100000;
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

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn grep_rejects_excessive_offset_before_pattern_compile() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        for offset in [100001, u64::MAX] {
            let req = serde_json::from_value(serde_json::json!({"id": "offset", "command": "grep", "pattern": "[", "offset": offset})).unwrap();
            let response = handle_grep(&req, &ctx);
            assert_eq!(
                response.data["code"], "invalid_request",
                "{:?}",
                response.data
            );
        }
        for offset in [0, 100000] {
            let req = serde_json::from_value(serde_json::json!({"id": "offset", "command": "grep", "pattern": "[", "offset": offset})).unwrap();
            assert_eq!(handle_grep(&req, &ctx).data["code"], "invalid_pattern");
        }
    }
}

/// Matching lines the engine keeps for one request: every row the offset
/// limit can reach, plus one page.
fn store_cap(project_root: &Path, page_size: usize) -> usize {
    #[cfg(test)]
    if let Some(cap) = work_probe::store_cap(project_root) {
        return cap;
    }
    #[cfg(not(test))]
    let _ = project_root;
    MAX_GREP_OFFSET.saturating_add(page_size)
}

/// Test-only counters and overrides for the page layout: they let tests
/// prove that its work grows with the rows involved, not with file sizes or
/// with the number of files times the deepest file, and shrink the store cap
/// so a capped search needs only a small fixture.
#[cfg(test)]
pub(crate) mod work_probe {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    thread_local! {
        /// Slots `round_robin_order` examined on this thread.
        pub(crate) static ROUND_ROBIN_VISITS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn visit_round_robin_slot() {
        ROUND_ROBIN_VISITS.with(|visits| visits.set(visits.get() + 1));
    }

    static STORE_CAPS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

    /// Reads of a file to window its long lines, and passes over its bytes
    /// to find the requested lines.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct FileWork {
        pub reads: usize,
        pub line_scans: usize,
    }

    static FILE_WORK: OnceLock<Mutex<HashMap<PathBuf, FileWork>>> = OnceLock::new();

    pub(crate) fn record(path: &Path, update: impl FnOnce(&mut FileWork)) {
        if let Ok(mut work) = FILE_WORK.get_or_init(Default::default).lock() {
            update(work.entry(path.to_path_buf()).or_default());
        }
    }

    pub(crate) fn file_work(path: &Path) -> FileWork {
        FILE_WORK
            .get_or_init(Default::default)
            .lock()
            .map(|work| work.get(path).copied().unwrap_or_default())
            .unwrap_or_default()
    }

    /// Store cap for searches whose project root is `root`.
    pub(crate) fn set_store_cap(root: &Path, cap: usize) {
        if let Ok(mut caps) = STORE_CAPS.get_or_init(Default::default).lock() {
            caps.insert(root.to_path_buf(), cap);
        }
    }

    pub(crate) fn store_cap(root: &Path) -> Option<usize> {
        STORE_CAPS
            .get_or_init(Default::default)
            .lock()
            .ok()
            .and_then(|caps| caps.get(root).copied())
    }
}

pub fn handle_grep(req: &RawRequest, ctx: &AppContext) -> Response {
    let config = ctx.config();
    let _pin = config.views.enabled.then(|| ctx.pin_config_to(config));
    ctx.with_read_policy_snapshot(&req.id, || {
        crate::context::confinement::with_floor_disclosure(|| {
            crate::bounded_io::command(req, ctx, handle_grep_inner)
        })
    })
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
    // `max_results`, when set, is the page size; `offset` skips that many
    // printed rows of the ordered result.
    let explicit_page_rows = req
        .params
        .get("max_results")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .filter(|value| *value > 0);
    let page_size = explicit_page_rows.unwrap_or(DEFAULT_MAX_RESULTS);
    let offset = req
        .params
        .get("offset")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    if offset > MAX_GREP_OFFSET as u64 {
        return Response::error(
            &req.id,
            "invalid_request",
            "grep: 'offset' must be at most 100000",
        );
    }
    let offset = offset as usize;
    // The engine counts and keeps every match up to this store cap, so totals
    // are exact and pages are cut from one fully sorted list. The cap keeps
    // memory bounded for a pattern that matches nearly every line; it covers
    // the last page the offset limit can reach, and a search that passes it
    // reports its total as a lower bound.
    let max_results = store_cap(&grep_executor::project_root(ctx), page_size);

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
        Some(crate::context::WorkerSearchTool::Grep),
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let project_root = grep_executor::project_root(ctx);
    crate::context::confinement::set_floor_filter(&project_root, &filters);
    // Search roots that are files: naming a file as `path` prints its lines
    // whatever kind of file it is.
    let file_roots: Vec<PathBuf> = scope
        .roots
        .iter()
        .filter(|root| crate::bounded_io::metadata(&root.search_root).is_ok_and(|m| m.is_file()))
        .map(|root| root.search_root.clone())
        .collect();
    let total_started = std::time::Instant::now();
    let search_start = std::time::Instant::now();
    let params = GrepParams {
        include,
        exclude,
        max_results,
        path_exclusion: None,
    };
    // Past the store cap, which lines are kept must not depend on how
    // parallel workers are scheduled, or two requests over unchanged files
    // could store different lines and their pages would overlap or skip rows.
    // A path-ordered scan keeps the first lines by path and line in one
    // parallel pass (see `GrepScanOrder::Path`).
    let order = GrepScanOrder::Path;
    // A parent folder session answers from its child repositories' indexes.
    // Child indexes intentionally omit ignored targets, just like the local
    // index. An explicitly named ignored root must use the direct walk instead.
    let parent_answer = if crate::context::confinement::active()
        || scope.roots.iter().any(|root| root.ignored_target)
    {
        None
    } else {
        crate::views::parent::grep_fan_out(ctx, &compiled, &scope, &params, &filters, order)
    };
    let (result, phases, parent_gaps) = match parent_answer {
        Some(answer) => answer,
        None => {
            let (result, phases) = grep_executor::execute_profiled_with_filters_in_order(
                ctx, &compiled, &scope, &params, &filters, order,
            );
            (result, phases, Vec::new())
        }
    };
    if result.index_sources_unavailable {
        return Response::error(&req.id, "internal_error", result.index_read_gaps.join("; "));
    }
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
    let output = grep_page_output(
        &result,
        &GrepPageRequest {
            project_root: &project_root,
            include: &params.include,
            file_roots: &file_roots,
            offset,
            explicit_page_rows,
        },
    );
    let next_offset = output.next_offset;
    let text = output.text.clone();
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
            && result.missing_on_disk == 0
            && result.index_read_gaps.is_empty(),
        "no_files_matched_scope": !scope_has_files,
        "skipped_foreign_mounts": result.skipped_foreign_mounts,
        "matches": output.page.iter().map(|m| match_to_json(m)).collect::<Vec<_>>(),
        "rendered_matches": output.rendered,
        "offset": offset,
        "output_byte_capped": output.rendered < output.page.len(),
        "lines_truncated": output.lines_truncated,
        "next_offset": next_offset,
        "total_matches": result.total_matches,
        "display_matches": output.display_total,
        "counted_matches": output.counted_matches,
        "counted_files": output.counted_files.iter().map(|(file, lines, kind)| serde_json::json!({
            "file": file,
            "lines": lines,
            "kind": kind.label(),
        })).collect::<Vec<_>>(),
        "files_searched": result.files_searched,
        "files_read_directly": result.files_read_directly,
        "files_with_matches": result.files_with_matches,
        "index_status": result.index_status.as_str(),
        "truncated": result.truncated,
        "search_ms": (search_ms * 1000.0).round() / 1000.0,
    });
    if !result.index_read_gaps.is_empty() {
        body["index_read_gap"] = serde_json::json!(result.index_read_gaps);
        if let Some(text) = body["text"].as_str() {
            body["text"] =
                serde_json::json!(format!("{text}\n\n{}", result.index_read_gaps.join("\n")));
        }
    }
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

    if let Some(mut envelope) = crate::list_surfaces::grep::build_grep_envelope(&body) {
        // `offset` is a knob only while this page has a next one: after the
        // last row, or once the accepted offset range is exhausted, offset
        // cannot reach more rows; `path` and `include` still can.
        if next_offset.is_none() {
            envelope.narrow.retain(|knob| knob != "offset");
        }
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

/// The footer of `format_grep_text`, which aft_search still prints. The grep
/// tool itself prints `grep_tool_footer`, with real plurals.
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
    format!(
        "Found {} match across {} file{}",
        result.total_matches,
        result.files_with_matches,
        grep_footer_tags(result)
    )
}

/// The tags `format_grep_text`'s footer puts after its counts: ` (capped)`
/// when the count is a floor, then ` [index: <status>]` unless the index was
/// ready. aft_search's ranked regex page ends its own count line with them.
pub(crate) fn grep_footer_tags(result: &GrepResult) -> String {
    let cap_note = if result.truncated { " (capped)" } else { "" };
    match result.index_status {
        IndexStatus::Ready => cap_note.to_string(),
        other => format!("{cap_note} [index: {}]", index_status_label(other)),
    }
}

/// The footer the grep tool prints. An index answer gets the plain footer.
/// Any other answer came from reading files directly (a fallback walk or a
/// named file), so its index label also says how much of the scope that read
/// covered: a zero next to "[index: building]" alone could mean either "no
/// match anywhere" or "the walk never got there", and the reader cannot tell
/// which without re-checking.
fn grep_tool_footer(result: &GrepResult) -> String {
    let found = found_matches_phrase(result.total_matches, result.files_with_matches);
    let cap_note = if result.truncated { " (capped)" } else { "" };
    if result.index_status == IndexStatus::Ready {
        return format!("{found}{cap_note}");
    }
    format!(
        "{found}{cap_note} [index: {}; {}]",
        index_status_label(result.index_status),
        direct_read_coverage(result)
    )
}

/// `Found 1 match in 1 file`, `Found 2 matches in 1 file`, `Found 3 matches
/// across 2 files`: the grep tool's count sentence, in plain English.
pub(crate) fn found_matches_phrase(matches: usize, files: usize) -> String {
    let matches = if matches == 1 {
        "1 match".to_string()
    } else {
        format!("{matches} matches")
    };
    let preposition = if files <= 1 { "in" } else { "across" };
    format!("Found {matches} {preposition} {}", plural(files, "file"))
}

/// How much of the scope a direct (non-index) read covered, for the footer.
fn direct_read_coverage(result: &GrepResult) -> String {
    let files = result.files_read_directly;
    if let Some(bound) = result.walk_bound {
        return format!(
            "incomplete: checked {} before {}",
            plural(files, "file"),
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
        return format!(
            "stopped at the match limit after searching {} directly",
            plural(files, "file")
        );
    }
    if files == 1 {
        return "searched 1 file directly".to_string();
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
            "checked {} ({index}); the fallback walk ran out of its time budget before reaching every file, so files it did not reach were not searched and matches may be missing.",
            plural(files, "file")
        ),
        WalkBound::Cancelled => format!(
            "checked {} ({index}); the request was cancelled before the fallback walk reached every file, so matches may be missing.",
            plural(files, "file")
        ),
    };
    Some(format!("({checked} {exhaustive})"))
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

/// Rows of the first page of a result too large to print whole.
const FIRST_PAGE_ROWS: usize = 40;
/// Byte budget for the rows of that first page.
const FIRST_PAGE_BYTES: usize = 12 * 1024;
/// A result with at most this many printed rows, whose rows also fit
/// `WHOLE_RESULT_BYTES`, prints whole in one reply with no file summary.
const WHOLE_RESULT_ROWS: usize = 100;
const WHOLE_RESULT_BYTES: usize = 16 * 1024;
/// Rows per file on the first page of a multi-file result. Rows are dealt
/// round-robin, one per file per round, and the first page stops after this
/// many rounds, so it samples every file instead of filling up from the few
/// files with the most lines.
const FIRST_PAGE_ROUNDS: usize = 5;
/// Files listed one by one in the files-first summary, most lines first.
const SUMMARY_FILES_LISTED: usize = 25;
/// Folders named in the summary's roll-up of the files not listed.
const SUMMARY_FOLDERS_LISTED: usize = 6;
/// Counted-only entries printed one per row. Past this, the first
/// `COUNTED_ENTRIES_LISTED - 1` are printed and the rest fold into one row
/// per kind and marker.
const COUNTED_ENTRIES_LISTED: usize = 8;
/// Counted-kind lines are printed instead of counted when there are this few
/// of them (and their text fits `HANDFUL_BYTES`): listing them would cost
/// about as much as printing them.
const HANDFUL_ROWS: usize = 10;
const HANDFUL_BYTES: usize = 2 * 1024;

/// One matching file and its matches, in result order.
struct MatchingFile<'a> {
    file: &'a Path,
    display: String,
    matches: Vec<&'a GrepMatch>,
    kind: FileKind,
}

impl MatchingFile<'_> {
    fn file_name(&self) -> String {
        self.file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// What the caller asked for that decides how a grep page is laid out.
pub(crate) struct GrepPageRequest<'a> {
    pub project_root: &'a Path,
    pub include: &'a [String],
    /// Search roots that are files: naming a file as `path` prints its lines
    /// whatever its kind.
    pub file_roots: &'a [PathBuf],
    pub offset: usize,
    /// `max_results` when the caller set it: every page then holds that many
    /// rows, with no first-page summary sizing.
    pub explicit_page_rows: Option<usize>,
}

/// One rendered grep page and the counts its JSON reply and list envelope
/// report.
pub(crate) struct GrepPageOutput<'a> {
    /// Reply text before the walk and index notes `handle_grep` appends.
    pub text: String,
    /// The page's rows in display order, before the byte budget applied.
    pub page: Vec<&'a GrepMatch>,
    /// Rows of `page` that fit the byte budget and were printed.
    pub rendered: usize,
    pub lines_truncated: usize,
    pub next_offset: Option<usize>,
    /// Rows grep prints across all pages: every match except the lines left
    /// counted only (counted kinds printed by the handful or only-counted
    /// rules are included).
    pub display_total: usize,
    /// Matching lines counted, not printed, and the files holding them.
    pub counted_matches: usize,
    pub counted_files: Vec<(String, usize, ContentKind)>,
}

/// Lay out one page of a grep result: decide which files print their lines
/// and which are only counted, deal rows round-robin by file, cut the page,
/// and write the text with its summary, counted-only block, footer and
/// continuation.
pub(crate) fn grep_page_output<'a>(
    result: &'a GrepResult,
    request: &GrepPageRequest<'_>,
) -> GrepPageOutput<'a> {
    let mut files = group_by_file(&result.matches, request.project_root);
    let mut kinds = FileKindCache::new();
    for file in &mut files {
        file.kind = kinds.classify(request.project_root, file.file, None);
    }

    let mut shown = Vec::new();
    let mut counted = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let targeted = request.file_roots.iter().any(|root| root == file.file)
            || include_targets(request.include, &file.kind, &file.file_name());
        if file.kind.content.is_counted() && !targeted {
            counted.push(index);
        } else {
            shown.push(index);
        }
    }

    let mut texts = RowTexts::default();
    let counted_rows: usize = counted
        .iter()
        .map(|&index| files[index].matches.len())
        .sum();
    // A handful of counted-kind lines costs about what listing them costs,
    // so they are printed after the other files instead.
    if !shown.is_empty() && counted_rows > 0 && counted_rows <= HANDFUL_ROWS {
        let rows: Vec<&GrepMatch> = counted
            .iter()
            .flat_map(|&index| files[index].matches.iter().copied())
            .collect();
        texts.prepare(&rows);
        let bytes: usize = rows.iter().map(|row| texts.text(row).len()).sum();
        if bytes <= HANDFUL_BYTES {
            shown.append(&mut counted);
        }
    }
    let mut only_counted_note = None;
    if shown.is_empty() && !counted.is_empty() {
        let mut kinds: Vec<&str> = counted
            .iter()
            .map(|&index| files[index].kind.content.plural())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        only_counted_note = Some(format!(
            "(Only {} matched, so their lines are shown.)",
            kinds.join(" and ")
        ));
        shown.append(&mut counted);
    }

    // Display order: round-robin by file, one row per file per round. A
    // page is a contiguous run of this order, so pages never overlap.
    let order = round_robin_order(&shown, &files);
    let display_total = order.len();
    let offset = request.offset;
    let first = offset == 0;
    let single_file = shown.len() == 1;
    let (row_cap, byte_cap, rounds) = if let Some(rows) = request.explicit_page_rows {
        (rows, GREP_MAX_OUTPUT_BYTES, None)
    } else if first {
        if display_total <= WHOLE_RESULT_ROWS
            && whole_result_bytes(&order, &files, &mut texts) <= WHOLE_RESULT_BYTES
        {
            (display_total, usize::MAX, None)
        } else {
            (
                FIRST_PAGE_ROWS,
                FIRST_PAGE_BYTES,
                (!single_file).then_some(FIRST_PAGE_ROUNDS),
            )
        }
    } else {
        (DEFAULT_MAX_RESULTS, GREP_MAX_OUTPUT_BYTES, None)
    };

    let page_slots: Vec<(usize, usize)> = order
        .iter()
        .skip(offset)
        .take_while(|(_, round)| rounds.is_none_or(|limit| *round < limit))
        .take(row_cap)
        .copied()
        .collect();
    let page: Vec<&GrepMatch> = page_slots
        .iter()
        .map(|&(file, round)| files[file].matches[round])
        .collect();
    texts.prepare(&page);

    // Cut the page at the byte budget, counting each file's header once.
    let mut rendered = 0usize;
    let mut bytes = 0usize;
    let mut headed = HashSet::new();
    for (slot, row) in page_slots.iter().zip(&page) {
        let mut cost = format!("{}: {}\n", row.line, texts.text(row)).len();
        if !headed.contains(&slot.0) {
            cost += files[slot.0].display.len() + 2;
        }
        if rendered > 0 && bytes + cost > byte_cap {
            break;
        }
        headed.insert(slot.0);
        bytes += cost;
        rendered += 1;
    }
    let next = offset.saturating_add(rendered);
    let next_offset =
        (rendered > 0 && next < display_total && next <= MAX_GREP_OFFSET).then_some(next);

    let mut sections = Vec::new();
    if first && next_offset.is_some() {
        sections.push(files_first_summary(&shown, &files, rendered));
    }
    if let Some(note) = only_counted_note {
        sections.push(note);
    }
    let mut lines_truncated = 0usize;
    if rendered > 0 {
        let (rows_text, cut) = page_rows_text(&page_slots[..rendered], &shown, &files, &texts);
        lines_truncated = cut;
        sections.push(rows_text);
    }
    let counted_matches: usize = counted
        .iter()
        .map(|&index| files[index].matches.len())
        .sum();
    if !counted.is_empty() {
        let lines = plural(counted_matches, "line");
        let in_files = plural(counted.len(), "file");
        if first {
            let mut block = format!(
                "Counted only, not shown: {lines} in {in_files}. To show them, pass a file below as path, or a pattern below as include."
            );
            for row in counted_entries(&counted, &files, request.project_root) {
                block.push('\n');
                block.push_str(&row);
            }
            sections.push(block);
        } else {
            sections.push(format!(
                "Counted only, not shown: {lines} in {in_files} (listed with offset=0)."
            ));
        }
    }
    sections.push(grep_tool_footer(result));
    if let Some(next) = next_offset {
        sections.push(format!("(More lines; continue with offset={next}.)"));
    } else if offset > 0 && rendered == 0 {
        sections.push(format!(
            "(offset={offset} is past the last match; nothing left to show.)"
        ));
    }

    GrepPageOutput {
        text: sections.join("\n\n"),
        page,
        rendered,
        lines_truncated,
        next_offset,
        display_total,
        counted_matches,
        counted_files: counted
            .iter()
            .map(|&index| {
                let file = &files[index];
                (file.display.clone(), file.matches.len(), file.kind.content)
            })
            .collect(),
    }
}

fn group_by_file<'a>(matches: &'a [GrepMatch], project_root: &Path) -> Vec<MatchingFile<'a>> {
    let mut files: Vec<MatchingFile<'a>> = Vec::new();
    let mut index_of: HashMap<&'a Path, usize> = HashMap::new();
    for grep_match in matches {
        let path = grep_match.file.as_path();
        let index = *index_of.entry(path).or_insert_with(|| {
            files.push(MatchingFile {
                file: path,
                display: display_path(path, project_root),
                matches: Vec::new(),
                kind: FileKind {
                    content: ContentKind::Text,
                    test: crate::file_kind::TestRole::None,
                    marker: None,
                },
            });
            files.len() - 1
        });
        files[index].matches.push(grep_match);
    }
    files
}

fn display_path(path: &Path, project_root: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// `(file, round)` slots, round by round: the first match of every shown
/// file, then the second of every file that has one, and so on. Each round
/// visits only the files that still have rows, so the work grows with the
/// number of stored matches, not with the number of files times the depth
/// of the busiest file.
fn round_robin_order(shown: &[usize], files: &[MatchingFile<'_>]) -> Vec<(usize, usize)> {
    let total: usize = shown.iter().map(|&index| files[index].matches.len()).sum();
    let mut order = Vec::with_capacity(total);
    let mut active: Vec<usize> = shown
        .iter()
        .copied()
        .filter(|&index| !files[index].matches.is_empty())
        .collect();
    let mut round = 0usize;
    while !active.is_empty() {
        for &index in &active {
            #[cfg(test)]
            work_probe::visit_round_robin_slot();
            order.push((index, round));
        }
        round += 1;
        active.retain(|&index| {
            #[cfg(test)]
            work_probe::visit_round_robin_slot();
            files[index].matches.len() > round
        });
    }
    order
}

fn whole_result_bytes(
    order: &[(usize, usize)],
    files: &[MatchingFile<'_>],
    texts: &mut RowTexts,
) -> usize {
    let rows: Vec<&GrepMatch> = order
        .iter()
        .map(|&(file, round)| files[file].matches[round])
        .collect();
    texts.prepare(&rows);
    let row_bytes: usize = rows
        .iter()
        .map(|row| format!("{}: {}\n", row.line, texts.text(row)).len())
        .sum();
    let mut headers = HashSet::new();
    let header_bytes: usize = order
        .iter()
        .filter(|(file, _)| headers.insert(*file))
        .map(|(file, _)| files[*file].display.len() + 2)
        .sum();
    row_bytes + header_bytes
}

/// The page's rows grouped under their file headers, files in display
/// order (`shown`) and rows in line order, plus how many printed lines were
/// cut.
fn page_rows_text(
    slots: &[(usize, usize)],
    shown: &[usize],
    files: &[MatchingFile<'_>],
    texts: &RowTexts,
) -> (String, usize) {
    let mut by_file: Vec<(usize, Vec<usize>)> = Vec::new();
    for &(file, round) in slots {
        match by_file.iter_mut().find(|(index, _)| *index == file) {
            Some((_, rounds)) => rounds.push(round),
            None => by_file.push((file, vec![round])),
        }
    }
    by_file.sort_by_key(|(file, _)| shown.iter().position(|index| index == file));
    let mut cut = 0usize;
    let blocks: Vec<String> = by_file
        .into_iter()
        .map(|(file, mut rounds)| {
            rounds.sort_unstable();
            let mut block = files[file].display.clone();
            for round in rounds {
                let row = files[file].matches[round];
                if line_text_was_cut(&row.line_text) {
                    cut += 1;
                }
                block.push('\n');
                block.push_str(&format!("{}: {}", row.line, texts.text(row)));
            }
            block
        })
        .collect();
    (blocks.join("\n\n"), cut)
}

/// `<S> lines in <F> files match; the first page shows <P>. Lines per file:`
/// then the files with the most lines, then a per-folder roll-up of the
/// rest, so the reader sees where every printed line is before paging.
fn files_first_summary(shown: &[usize], files: &[MatchingFile<'_>], rendered: usize) -> String {
    let total_rows: usize = shown.iter().map(|&index| files[index].matches.len()).sum();
    let mut summary = format!(
        "{} in {} {}; the first page shows {rendered}. Lines per file:",
        plural(total_rows, "line"),
        plural(shown.len(), "file"),
        if total_rows == 1 { "matches" } else { "match" },
    );
    let mut ranked: Vec<usize> = shown.to_vec();
    // Stable: files with equal counts keep result order.
    ranked.sort_by_key(|&index| std::cmp::Reverse(files[index].matches.len()));
    let (listed, rest) = ranked.split_at(ranked.len().min(SUMMARY_FILES_LISTED));
    for &index in listed {
        summary.push_str(&format!(
            "\n{} {}",
            files[index].display,
            files[index].matches.len()
        ));
    }
    if !rest.is_empty() {
        let mut folders: Vec<(String, usize, usize)> = Vec::new();
        for &index in rest {
            let folder = summary_folder(&files[index].display);
            let lines = files[index].matches.len();
            match folders.iter_mut().find(|(name, _, _)| *name == folder) {
                Some((_, count, total)) => {
                    *count += 1;
                    *total += lines;
                }
                None => folders.push((folder, 1, lines)),
            }
        }
        folders.sort_by_key(|(_, _, lines)| std::cmp::Reverse(*lines));
        let mut parts: Vec<String> = folders
            .iter()
            .take(SUMMARY_FOLDERS_LISTED)
            .map(|(folder, count, lines)| format!("{folder} {count}/{lines}"))
            .collect();
        let others = &folders[folders.len().min(SUMMARY_FOLDERS_LISTED)..];
        if !others.is_empty() {
            let count: usize = others.iter().map(|(_, count, _)| count).sum();
            let lines: usize = others.iter().map(|(_, _, lines)| lines).sum();
            parts.push(format!("other folders {count}/{lines}"));
        }
        let rest_lines: usize = rest.iter().map(|&index| files[index].matches.len()).sum();
        summary.push_str(&format!(
            "\n+{} more {} ({}), files/lines per folder: {}",
            rest.len(),
            if rest.len() == 1 { "file" } else { "files" },
            plural(rest_lines, "line"),
            parts.join(", ")
        ));
    }
    summary
}

/// The folder a summary roll-up counts a file under: its first two parent
/// folders (`packages/core/`), or `./` for a file at the top, written with
/// the platform's separator like the paths grep prints (`packages\core\` on
/// Windows). The printed path is split in the `/` form of
/// [`include_glob_form`], because Windows prints `\` and a split on `/`
/// alone would put every file under `./`.
fn summary_folder(display: &str) -> String {
    let slashed = include_glob_form(display);
    let parents: Vec<&str> = slashed.split('/').collect();
    let parents = &parents[..parents.len().saturating_sub(1)];
    let separator = std::path::MAIN_SEPARATOR_STR;
    if parents.is_empty() {
        format!(".{separator}")
    } else {
        format!(
            "{}{separator}",
            parents[..parents.len().min(2)].join(separator)
        )
    }
}

/// One counted-only entry: a file by its path, or a pattern that names
/// several files of one kind.
struct CountedEntry {
    label: String,
    content: ContentKind,
    lines: usize,
    /// The counted files this entry stands for.
    members: Vec<usize>,
    marker: Option<&'static str>,
}

fn counted_kind_rank(content: ContentKind) -> u8 {
    // Lockfiles first: a version question is the likeliest reason to want
    // a counted line.
    match content {
        ContentKind::Lockfile => 0,
        ContentKind::DataJson => 1,
        ContentKind::Generated | ContentKind::Minified => 2,
        ContentKind::Capture | ContentKind::Text => 3,
    }
}

/// Rows of the counted-only block. Every row names a `path` or an
/// `include` value that prints its files; a pattern is used only when it
/// is checked to name every file it stands for.
fn counted_entries(
    counted: &[usize],
    files: &[MatchingFile<'_>],
    project_root: &Path,
) -> Vec<String> {
    let mut entries = Vec::new();
    let mut singles: Vec<usize> = Vec::new();

    // Files of one kind that share a name fold to `**/<name>`.
    let mut by_name: Vec<((ContentKind, String), Vec<usize>)> = Vec::new();
    for &index in counted {
        let key = (files[index].kind.content, files[index].file_name());
        match by_name.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(index),
            None => by_name.push((key, vec![index])),
        }
    }
    for ((content, name), members) in by_name {
        let label = format!("**/{name}");
        if members.len() > 1 && pattern_names_all(&label, &members, files, project_root) {
            entries.push(entry_for(label, content, &members, files));
        } else {
            singles.extend(members);
        }
    }

    // Files of one kind and marker in one folder fold to
    // `<folder>/*<shared suffix>`.
    let mut by_folder: Vec<((ContentKind, String, Option<&'static str>), Vec<usize>)> = Vec::new();
    for index in singles {
        // The folder goes into an `include` glob, which grep matches against
        // the `/`-separated relative path; a Windows `\` would be read as a
        // literal character and select nothing.
        let display = include_glob_form(&files[index].display);
        let folder = display
            .rsplit_once('/')
            .map(|(folder, _)| folder.to_string())
            .unwrap_or_default();
        let key = (files[index].kind.content, folder, files[index].kind.marker);
        match by_folder.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(index),
            None => by_folder.push((key, vec![index])),
        }
    }
    for ((content, folder, _), members) in by_folder {
        if members.len() > 1 {
            let names: Vec<String> = members
                .iter()
                .map(|&index| files[index].file_name())
                .collect();
            let suffix = shared_dotted_suffix(&names);
            let label = if folder.is_empty() {
                format!("*{suffix}")
            } else {
                format!("{folder}/*{suffix}")
            };
            if pattern_names_all(&label, &members, files, project_root) {
                entries.push(entry_for(label, content, &members, files));
                continue;
            }
        }
        for index in members {
            entries.push(entry_for(
                files[index].display.clone(),
                content,
                &[index],
                files,
            ));
        }
    }

    entries.sort_by(|a, b| {
        counted_kind_rank(a.content)
            .cmp(&counted_kind_rank(b.content))
            .then(b.lines.cmp(&a.lines))
            .then(a.label.cmp(&b.label))
    });
    if entries.len() <= COUNTED_ENTRIES_LISTED {
        return entries.iter().map(entry_row).collect();
    }
    let (listed, rest) = entries.split_at(COUNTED_ENTRIES_LISTED - 1);
    let mut rows: Vec<String> = listed.iter().map(entry_row).collect();
    // The rest fold into one row per kind and marker. A row offers an include
    // pattern only when the pattern is checked to select every folded file;
    // otherwise it names the files themselves, so every counted line keeps a
    // value that shows it.
    let mut folded: Vec<((ContentKind, Option<&'static str>), usize, Vec<usize>)> = Vec::new();
    for entry in rest {
        let key = (entry.content, entry.marker);
        match folded.iter_mut().find(|(existing, _, _)| *existing == key) {
            Some((_, lines, members)) => {
                *lines += entry.lines;
                members.extend(&entry.members);
            }
            None => folded.push((key, entry.lines, entry.members.clone())),
        }
    }
    for ((content, marker), lines, members) in folded {
        let count = members.len();
        let what = if count == 1 && content == ContentKind::Lockfile {
            content.label().to_string()
        } else if count == 1 {
            format!("{} file", content.label())
        } else {
            content.plural().to_string()
        };
        let include = folded_include_candidates(content, marker)
            .into_iter()
            .find(|pattern| pattern_names_all(pattern, &members, files, project_root));
        let hint = match include {
            Some(pattern) => format!("include {pattern}"),
            None => format!(
                "paths {}",
                members
                    .iter()
                    .map(|&index| files[index].display.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        rows.push(format!(
            "+{count} more {what}: {}; {hint}",
            plural(lines, "line")
        ));
    }
    rows
}

/// Include patterns that could select every file of one kind and marker,
/// most specific first. Files decided by path shape or content have no
/// marker, so no pattern names them.
fn folded_include_candidates(content: ContentKind, marker: Option<&'static str>) -> Vec<String> {
    let Some(marker) = marker else {
        return Vec::new();
    };
    if marker.starts_with('.') {
        // `**/*.log*` also selects rotated logs (`app.log.1`).
        vec![format!("**/*{marker}"), format!("**/*{marker}*")]
    } else if content == ContentKind::Lockfile {
        vec![format!("**/{marker}")]
    } else {
        vec![format!("**/*{marker}*")]
    }
}

/// `display` spelled the way `include` globs are matched: grep compares an
/// `include` against the project-relative path with `/` separators, while
/// Windows prints paths with `\`. Elsewhere `\` can be part of a file name,
/// so the path is left as it is.
fn include_glob_form(display: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) {
        std::borrow::Cow::Owned(display.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(display)
    }
}

fn entry_for(
    label: String,
    content: ContentKind,
    members: &[usize],
    files: &[MatchingFile<'_>],
) -> CountedEntry {
    let marker = files[members[0]].kind.marker;
    CountedEntry {
        label,
        content,
        lines: members
            .iter()
            .map(|&index| files[index].matches.len())
            .sum(),
        members: members.to_vec(),
        marker: members
            .iter()
            .all(|&index| files[index].kind.marker == marker)
            .then_some(marker)
            .flatten(),
    }
}

fn entry_row(entry: &CountedEntry) -> String {
    let in_files = if entry.members.len() > 1 {
        format!(" in {} files", entry.members.len())
    } else {
        String::new()
    };
    format!(
        "{}: {}{in_files} ({})",
        entry.label,
        plural(entry.lines, "line"),
        entry.content.label()
    )
}

/// Whether passing `pattern` as `include` prints every member's lines: the
/// glob selects each member's path (relative to the project root, as grep
/// matches `include`), and the pattern names its kind (see
/// [`include_targets`]). An empty member list is never named.
fn pattern_names_all(
    pattern: &str,
    members: &[usize],
    files: &[MatchingFile<'_>],
    project_root: &Path,
) -> bool {
    let patterns = [pattern.to_string()];
    let Ok(filters) = build_path_filters(&patterns, &[]) else {
        return false;
    };
    !members.is_empty()
        && members.iter().all(|&index| {
            filters.matches(project_root, files[index].file)
                && include_targets(&patterns, &files[index].kind, &files[index].file_name())
        })
}

/// The longest suffix all names share, starting at a dot (`.json`,
/// `.stdout.txt`), or empty when they share none.
fn shared_dotted_suffix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut shared = first.as_str();
    for name in &names[1..] {
        let common = shared
            .chars()
            .rev()
            .zip(name.chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum::<usize>();
        shared = &shared[shared.len() - common..];
    }
    shared
        .find('.')
        .map(|dot| shared[dot..].to_string())
        .unwrap_or_default()
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// Whether an `include` value asks for a counted file: some pattern's last
/// `/`-separated part (each brace alternative separately) either is the
/// file's exact name, or, with glob characters removed, ends with the file's
/// marker when that is a suffix (`*.log`, `*.min.js`) or contains it when it
/// is a whole name or a name segment (`**/bun.lock`, `**/*stdout*`). A part
/// made only of wildcards, or a broader extension (`*.js` for a `.min.js`
/// file), does not. A file without a marker is decided by its path shape or
/// content, which no pattern names; only `path` naming the file prints it.
pub(crate) fn include_targets(includes: &[String], kind: &FileKind, file_name: &str) -> bool {
    let Some(marker) = kind.marker else {
        return false;
    };
    let marker = marker.to_ascii_lowercase();
    includes.iter().any(|pattern| {
        let trimmed = pattern.trim_end_matches('/');
        let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
        expand_braces(last).iter().any(|alternative| {
            if !alternative.contains(['*', '?', '[']) && alternative.eq_ignore_ascii_case(file_name)
            {
                return true;
            }
            let literal: String = alternative
                .chars()
                .filter(|c| !matches!(c, '*' | '?' | '[' | ']'))
                .collect::<String>()
                .to_ascii_lowercase();
            if kind.marker_is_suffix() {
                literal.ends_with(&marker)
            } else {
                literal.contains(&marker)
            }
        })
    })
}

/// Every alternative of a glob part with `{a,b}` groups (nested too).
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let mut depth = 0usize;
    let mut close = None;
    let mut splits = Vec::new();
    for (index, ch) in pattern[open..].char_indices() {
        let index = open + index;
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            ',' if depth == 1 => splits.push(index),
            _ => {}
        }
    }
    let Some(close) = close else {
        return vec![pattern.to_string()];
    };
    let (head, tail) = (&pattern[..open], &pattern[close + 1..]);
    let mut bounds = vec![open];
    bounds.extend(splits);
    bounds.push(close);
    bounds
        .windows(2)
        .flat_map(|pair| {
            let choice = &pattern[pair[0] + 1..pair[1]];
            expand_braces(&format!("{head}{choice}{tail}"))
        })
        .collect()
}

/// Whether the engine kept only the start of a long line: its first
/// `GREP_MAX_LINE_CHARS` characters and the truncation marker.
fn line_text_was_cut(line_text: &str) -> bool {
    line_text
        .strip_suffix(GREP_LINE_TRUNCATED_MARKER)
        .is_some_and(|kept| kept.chars().count() == GREP_MAX_LINE_CHARS)
}

/// The printed text of each row: the whole line when it is short, else a
/// window of `GREP_MAX_LINE_CHARS` characters around the match. The engine
/// keeps only the start of a long line, so a match past column 1 is windowed
/// from the line read again from disk, once per file per batch of rows; if
/// the file cannot be read again, the kept start of the line is printed.
#[derive(Default)]
struct RowTexts {
    texts: HashMap<(PathBuf, u32), String>,
}

impl RowTexts {
    fn prepare(&mut self, rows: &[&GrepMatch]) {
        let mut reread: HashMap<&Path, Vec<&GrepMatch>> = HashMap::new();
        for row in rows {
            let key = (row.file.clone(), row.line);
            if self.texts.contains_key(&key) {
                continue;
            }
            if line_text_was_cut(&row.line_text) && row.column > 1 {
                reread.entry(row.file.as_path()).or_default().push(row);
            } else {
                self.texts.insert(key, row.line_text.clone());
            }
        }
        for (file, rows) in reread {
            let content = crate::project_content::read_bounded(file, None, false).ok();
            #[cfg(test)]
            work_probe::record(file, |work| work.reads += 1);
            // Every requested line of this file is located in one pass.
            let lines: Vec<u32> = rows.iter().map(|row| row.line).collect();
            let spans = content
                .as_deref()
                .map(|bytes| {
                    #[cfg(test)]
                    work_probe::record(file, |work| work.line_scans += 1);
                    line_spans(bytes, &lines)
                })
                .unwrap_or_default();
            for row in rows {
                let text = content
                    .as_deref()
                    .zip(spans.get(&row.line))
                    .map(|(bytes, &(start, end))| windowed_line(&bytes[start..end], row.column))
                    .unwrap_or_else(|| row.line_text.clone());
                self.texts.insert((row.file.clone(), row.line), text);
            }
        }
    }

    fn text<'r>(&'r self, row: &'r GrepMatch) -> &'r str {
        self.texts
            .get(&(row.file.clone(), row.line))
            .map(String::as_str)
            .unwrap_or(&row.line_text)
    }
}

/// Byte ranges (without the newline) of the requested 1-based `lines` of
/// `content`, found in one pass over it. Lines past the end are left out.
fn line_spans(content: &[u8], lines: &[u32]) -> HashMap<u32, (usize, usize)> {
    let mut wanted = lines.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let mut wanted = wanted.into_iter().peekable();
    let mut spans = HashMap::new();
    let mut newlines = memchr::memchr_iter(b'\n', content);
    let mut line = 1u32;
    let mut start = 0usize;
    while let Some(&next) = wanted.peek() {
        let end = newlines.next().unwrap_or(content.len());
        if line == next {
            spans.insert(line, (start, end));
            wanted.next();
        }
        if end >= content.len() {
            break;
        }
        start = end + 1;
        line += 1;
    }
    spans
}

/// One line's bytes, cut to a window around the character at 1-based
/// `column`.
fn windowed_line(line: &[u8], column: u32) -> String {
    let mut bytes = line;
    if bytes.ends_with(b"\r") {
        bytes = &bytes[..bytes.len() - 1];
    }
    let text = String::from_utf8_lossy(bytes);
    let anchor = text
        .char_indices()
        .nth(column.saturating_sub(1) as usize)
        .map_or(text.len(), |(index, _)| index);
    crate::commands::semantic_search::snippet_bounds::window_snippet_line(&text, anchor)
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
            index_sources_unavailable: false,
            index_read_gaps: Vec::new(),
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
    fn grep_offset_ceiling_ends_paging_with_a_narrowing_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("matches.txt");
        std::fs::write(&file, "needle\n".repeat(100003)).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        for (offset, size, next) in [
            (99999, 1, Some(100000)),
            (100000, 1, None),
            (99999, 2, None),
        ] {
            let req = serde_json::from_value(serde_json::json!({
                "id": "offset-ceiling", "command": "grep", "path": file,
                "pattern": "needle", "offset": offset, "max_results": size,
            }))
            .unwrap();
            let response = handle_grep(&req, &ctx);
            assert!(response.success, "{:?}", response.data);
            assert_eq!(response.data["rendered_matches"], size);
            let text = crate::subc_format::format_response("grep", &response, false);
            if let Some(next) = next {
                assert_eq!(response.data["next_offset"], next);
                assert!(
                    text.contains(&format!("continue with offset={next}")),
                    "{text}"
                );
                continue;
            }
            assert!(
                response.data["next_offset"].is_null(),
                "{:?}",
                response.data
            );
            let trailer = text.lines().last().unwrap();
            assert!(trailer.starts_with(&format!("shown {size} of ")), "{text}");
            assert!(
                trailer.ends_with(" rows (cap) · narrow: path, include"),
                "{text}"
            );
            assert!(!text.contains("continue with offset="), "{text}");
        }
    }

    #[test]
    fn round_robin_work_grows_with_stored_matches_not_with_files_times_depth() {
        // One busy file and many single-match files: dealing rows must not
        // revisit every file for every round of the busiest one.
        let mut matches = Vec::with_capacity(100_000);
        for line in 1..=50_000 {
            matches.push(grep_match("/project/busy.rs", line, "needle"));
        }
        for index in 0..49_999 {
            matches.push(grep_match(
                &format!("/project/single_{index}.rs"),
                1,
                "needle",
            ));
        }
        let files = group_by_file(&matches, &root());
        let shown: Vec<usize> = (0..files.len()).collect();
        work_probe::ROUND_ROBIN_VISITS.with(|visits| visits.set(0));
        let order = round_robin_order(&shown, &files);
        let visits = work_probe::ROUND_ROBIN_VISITS.with(|visits| visits.get());
        assert_eq!(order.len(), matches.len());
        assert!(
            visits <= 2 * matches.len(),
            "visited {visits} slots for {} stored matches",
            matches.len()
        );
        // Round one deals the first row of every file, in file order.
        assert!(order[..files.len()]
            .iter()
            .enumerate()
            .all(|(position, &(file, round))| file == position && round == 0));
        assert!(order[files.len()..]
            .iter()
            .enumerate()
            .all(|(position, &(file, round))| file == 0 && round == position + 1));
    }

    #[test]
    fn capped_search_keeps_the_same_rows_whatever_order_files_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for index in 0..12 {
            std::fs::write(
                root.join(format!("part_{index:02}.txt")),
                "needle\n".repeat(10),
            )
            .unwrap();
        }
        // 120 matches against a store cap of 50: which 50 are kept must not
        // depend on the order the files reach the search.
        work_probe::set_store_cap(&root, 50);
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        );
        let stored_rows = |reversed: bool| {
            crate::grep_executor::scan_order_for_test::set(&root, reversed);
            let req = serde_json::from_value(serde_json::json!({
                "id": "capped-order", "command": "grep", "pattern": "needle",
                "max_results": 1000,
            }))
            .unwrap();
            let response = handle_grep(&req, &ctx);
            assert!(response.success, "{:?}", response.data);
            assert_eq!(response.data["truncated"], true, "{:?}", response.data);
            response.data["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    (
                        row["file"].as_str().unwrap().to_string(),
                        row["line"].as_u64().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let forward = stored_rows(false);
        let reversed = stored_rows(true);
        assert_eq!(forward.len(), 50);
        assert_eq!(forward, reversed);
    }

    #[test]
    fn path_ordered_indexed_scan_keeps_the_first_rows_by_path_on_any_thread_count() {
        // 600 files (three batches) of 3 matches against a cap of 1,000: the
        // kept rows must be the first 1,000 by path and line, however many
        // threads finish the files of a batch in whatever order, and whatever
        // order the index lists the candidate files in.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for index in 0..600 {
            std::fs::write(root.join(format!("f{index:04}.txt")), "needle\n".repeat(3)).unwrap();
        }
        let index = crate::search_index::SearchIndex::build(&root);
        let snapshot = index.snapshot();
        let pattern = compiled_literal("needle");
        let query = crate::search_index::decompose_grep_pattern(&pattern);
        let filters = build_path_filters(&[], &[]).unwrap();
        let every_row: Vec<(PathBuf, u32)> = (0..600)
            .flat_map(|index| {
                let file = root.join(format!("f{index:04}.txt"));
                (1..=3).map(move |line| (file.clone(), line))
            })
            .collect();
        let expected = &every_row[..1000];
        for (threads, reversed) in [(1, false), (8, false), (1, true), (8, true)] {
            crate::grep_executor::scan_order_for_test::set(&root, reversed);
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let (result, _) = pool
                .install(|| {
                    snapshot.search_grep_profiled_with_filters_and_query_in_order(
                        &pattern,
                        &query,
                        &filters,
                        &root,
                        1000,
                        None,
                        GrepScanOrder::Path,
                    )
                })
                .expect("grep over a test index");
            assert!(result.truncated, "{threads} threads, reversed {reversed}");
            let mut kept: Vec<(PathBuf, u32)> = result
                .matches
                .iter()
                .map(|row| (row.file.clone(), row.line))
                .collect();
            kept.sort();
            assert_eq!(kept, expected, "{threads} threads, reversed {reversed}");
        }
    }

    #[test]
    fn path_ordered_indexed_scan_of_dense_files_holds_at_most_its_row_budget() {
        // 256 files of 100,000 matching lines each against the default store
        // cap of 100,100. Every file of the batch keeping all the lines the
        // cap needs would hold 25.6 million rows to keep 100,100.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let content = "x\n".repeat(100_000);
        for index in 0..256 {
            std::fs::write(root.join(format!("dense_{index:03}.txt")), &content).unwrap();
        }
        let index = crate::search_index::SearchIndex::build(&root);
        let snapshot = index.snapshot();
        let pattern = compiled_literal("x");
        let query = crate::search_index::decompose_grep_pattern(&pattern);
        let filters = build_path_filters(&[], &[]).unwrap();
        let max_results = 100_100;
        crate::search_index::path_order_held_rows_for_test::take(&root);
        let (result, _) = snapshot
            .search_grep_profiled_with_filters_and_query_in_order(
                &pattern,
                &query,
                &filters,
                &root,
                max_results,
                None,
                GrepScanOrder::Path,
            )
            .expect("grep over a test index");
        let peak = crate::search_index::path_order_held_rows_for_test::take(&root);
        assert!(
            peak <= max_results + crate::search_index::PATH_ORDER_BATCH_ROW_BUDGET,
            "held {peak} rows at once"
        );
        assert!(!result.scan_deadline_reached, "{result:?}");
        assert!(result.truncated);
        let first = root.join("dense_000.txt");
        let second = root.join("dense_001.txt");
        let expected: Vec<(PathBuf, u32)> = (1..=100_000)
            .map(|line| (first.clone(), line))
            .chain((1..=100).map(|line| (second.clone(), line)))
            .collect();
        let mut kept: Vec<(PathBuf, u32)> = result
            .matches
            .iter()
            .map(|row| (row.file.clone(), row.line))
            .collect();
        kept.sort();
        assert_eq!(kept, expected);
    }

    #[test]
    fn path_ordered_batch_keeps_the_same_rows_as_keeping_everything_the_cap_needs() {
        // Files of 0 to 7 matching lines, each keeping at most 2 in the batch:
        // whatever the cap, the kept rows must be the first by file and line,
        // as if every file had kept all the lines the cap still needs.
        let line_counts = [3usize, 0, 1, 7, 2, 5, 0, 4];
        let every_row: Vec<(usize, usize)> = line_counts
            .iter()
            .enumerate()
            .flat_map(|(file, &count)| (1..=count).map(move |line| (file, line)))
            .collect();
        let rows_of = |file: usize, limit: usize| {
            let rows: Vec<(usize, usize)> = (1..=line_counts[file].min(limit))
                .map(|line| (file, line))
                .collect();
            (rows, line_counts[file] > limit)
        };
        let allowance = 2;
        for max_results in 1..=every_row.len() + 1 {
            let per_file = (0..line_counts.len())
                .map(|file| rows_of(file, allowance.min(max_results)))
                .collect();
            let mut searched_again = Vec::new();
            let mut peak = 0;
            let mut kept = Vec::new();
            let lines_past = crate::search_index::keep_path_ordered_batch(
                &mut kept,
                max_results,
                allowance.min(max_results),
                per_file,
                |file, need| {
                    searched_again.push((file, need));
                    rows_of(file, need)
                },
                |rows| peak = peak.max(rows),
            );
            let expected = &every_row[..max_results.min(every_row.len())];
            assert_eq!(kept, expected, "cap {max_results}");
            assert_eq!(
                lines_past,
                max_results < every_row.len(),
                "cap {max_results}"
            );
            // Only a file the cap needs more than two lines from is searched
            // again, for exactly the lines still needed.
            for &(file, need) in &searched_again {
                let before = every_row
                    .iter()
                    .filter(|(row_file, _)| *row_file < file)
                    .count();
                assert_eq!(need, max_results - before, "cap {max_results}");
                assert!(line_counts[file] > 2 && need > 2, "cap {max_results}");
            }
            assert!(
                peak <= max_results + 2 * line_counts.len(),
                "cap {max_results}"
            );
        }
    }

    #[test]
    fn line_spans_finds_every_requested_line_in_one_pass() {
        let content = b"a\nbb\r\nccc";
        let spans = line_spans(content, &[3, 1, 2, 9, 2]);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[&1], (0, 1));
        assert_eq!(spans[&2], (2, 5));
        assert_eq!(spans[&3], (6, 9));
        assert_eq!(windowed_line(&content[2..5], 1), "bb");
    }

    #[test]
    fn a_page_of_long_rows_reads_and_scans_its_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("wide.txt");
        let line = format!("{}needle{}\n", "x".repeat(600), "y".repeat(400));
        std::fs::write(&file, line.repeat(200)).unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        );
        let req = serde_json::from_value(serde_json::json!({
            "id": "long-rows", "command": "grep", "pattern": "needle",
            "path": file, "max_results": 100,
        }))
        .unwrap();
        let response = handle_grep(&req, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["matches"].as_array().unwrap().len(), 100);
        let searched = PathBuf::from(response.data["matches"][0]["file"].as_str().unwrap());
        assert_eq!(
            work_probe::file_work(&searched),
            work_probe::FileWork {
                reads: 1,
                line_scans: 1
            }
        );
    }

    #[test]
    fn count_phrases_use_singular_and_plural_forms() {
        assert_eq!(found_matches_phrase(1, 1), "Found 1 match in 1 file");
        assert_eq!(found_matches_phrase(2, 1), "Found 2 matches in 1 file");
        assert_eq!(found_matches_phrase(3, 2), "Found 3 matches across 2 files");
        assert_eq!(found_matches_phrase(0, 0), "Found 0 matches in 0 files");

        let mut one = grep_result(
            vec![grep_match("/project/a.rs", 1, "needle")],
            1,
            1,
            1,
            IndexStatus::Fallback,
            false,
        );
        one.files_read_directly = 1;
        assert_eq!(
            grep_tool_footer(&one),
            "Found 1 match in 1 file [index: fallback; searched 1 file directly]"
        );
        one.walk_bound = Some(WalkBound::FileLimit(1));
        assert_eq!(
            grep_tool_footer(&one),
            "Found 1 match in 1 file [index: fallback; incomplete: checked 1 file before the walk's 1-file limit]"
        );
        one.walk_bound = Some(WalkBound::TimeBudget);
        assert!(walk_coverage_note(&one)
            .expect("note")
            .starts_with("(checked 1 file (index not used);"));

        // A one-line summary and the folded counted-only rows use the
        // singular too.
        let matches = vec![grep_match("/project/a.rs", 1, "needle")];
        let files = group_by_file(&matches, &root());
        assert!(files_first_summary(&[0], &files, 1)
            .starts_with("1 line in 1 file matches; the first page shows 1."));

        let mut counted_matches: Vec<GrepMatch> = (0..8)
            .map(|index| grep_match(&format!("/project/d{index}/rec{index}.json"), 1, "needle"))
            .collect();
        counted_matches.push(grep_match("/project/logs/run.log", 1, "needle"));
        let mut files = group_by_file(&counted_matches, &root());
        for file in &mut files[..8] {
            file.kind.content = ContentKind::DataJson;
            file.kind.marker = Some(".json");
        }
        files[8].kind.content = ContentKind::Capture;
        files[8].kind.marker = Some(".log");
        let indexes: Vec<usize> = (0..files.len()).collect();
        let rows = counted_entries(&indexes, &files, &root());
        assert_eq!(rows.len(), COUNTED_ENTRIES_LISTED + 1, "{rows:?}");
        assert!(rows[0].ends_with(": 1 line (JSON data)"), "{rows:?}");
        assert_eq!(
            rows[COUNTED_ENTRIES_LISTED - 1],
            "+1 more JSON data file: 1 line; include **/*.json"
        );
        assert_eq!(
            rows[COUNTED_ENTRIES_LISTED],
            "+1 more log or command output file: 1 line; include **/*.log"
        );
    }

    #[test]
    fn include_targets_names_the_file_or_its_marker_only() {
        let root = Path::new("/project");
        let kind = |relative: &str| {
            crate::file_kind::classify(root, &root.join(relative), Some(b"".as_slice()))
        };
        let targets = |pattern: &str, relative: &str| {
            let name = relative.rsplit('/').next().unwrap();
            include_targets(&[pattern.to_string()], &kind(relative), name)
        };
        assert!(targets("bun.lock", "tools/bun.lock"));
        assert!(targets("**/bun.lock", "tools/bun.lock"));
        assert!(targets("*.log", "runs/app.log"));
        assert!(targets("**/*.{ts,log}", "runs/app.log"));
        assert!(targets("**/stdout", "runs/io/stdout"));
        assert!(targets("**/*stderr*", "runs/io/stderr"));
        assert!(targets(
            "research/x/receipts/*.stdout.txt",
            "research/x/receipts/bash-1.stdout.txt"
        ));
        assert!(targets("*.min.js", "vendor/highlight.min.js"));
        // Wildcards alone and broader extensions do not name the kind.
        assert!(!targets("**/*", "runs/app.log"));
        assert!(!targets("*.js", "vendor/highlight.min.js"));
        assert!(!targets("*.txt", "runs/stdout.txt"));
        // A kind decided by path shape has no marker: only `path` names it.
        assert!(!targets("**/*.ts", "src/generated/api.ts"));
    }

    #[test]
    fn files_first_summary_rolls_up_files_past_the_listed_ones() {
        let mut matches = vec![
            grep_match("/project/src/busy.rs", 1, "needle"),
            grep_match("/project/src/busy.rs", 2, "needle"),
        ];
        for index in 0..26 {
            let folder = if index < 20 { "pkg/a/deep" } else { "top" };
            matches.push(grep_match(
                &format!("/project/{folder}/file_{index}.rs"),
                1,
                "needle",
            ));
        }
        matches.push(grep_match("/project/README.md", 1, "needle"));
        let files = group_by_file(&matches, &root());
        let shown: Vec<usize> = (0..files.len()).collect();
        let summary = files_first_summary(&shown, &files, 40);
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(
            lines[0],
            "29 lines in 28 files match; the first page shows 40. Lines per file:"
        );
        assert_eq!(lines[1], "src/busy.rs 2");
        assert_eq!(lines.len(), 1 + SUMMARY_FILES_LISTED + 1, "{summary}");
        // Files past the 25 listed roll up by their first two folders, and a
        // file at the top counts under `./`. Folder labels use the platform's
        // separator, so Windows prints `top\` and `.\`.
        let separator = std::path::MAIN_SEPARATOR_STR;
        assert_eq!(
            lines[lines.len() - 1],
            format!(
                "+3 more files (3 lines), files/lines per folder: top{separator} 2/2, .{separator} 1/1"
            )
        );
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
            let legacy = snapshot
                .search_grep(&pattern, &include, &exclude, project.path(), max_results)
                .unwrap();
            let filters = build_path_filters(&include, &exclude).expect("valid filters");
            let optimized = snapshot
                .search_grep_profiled_with_filters(
                    &pattern,
                    &filters,
                    project.path(),
                    max_results,
                    None,
                )
                .unwrap()
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
            let result = snapshot
                .search_grep(&pattern, &include, &exclude, &project_root, 100)
                .unwrap();
            std::hint::black_box(result.total_matches);
        };
        let optimized_once = || {
            let filters = build_path_filters(&include, &exclude).expect("valid filters");
            let result = snapshot
                .search_grep_profiled_with_filters(&pattern, &filters, &project_root, 100, None)
                .unwrap()
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
