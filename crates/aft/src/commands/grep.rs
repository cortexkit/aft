use std::collections::HashMap;
use std::path::Path;

use crate::context::AppContext;
use crate::grep_executor::{self, GrepParams};
use crate::pattern_compile::{self, CompileOpts, CompileResult};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{build_path_filters, GrepMatch, GrepResult, IndexStatus};

pub(crate) use crate::grep_executor::ripgrep_glob;

const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_LINE_CHARS: usize = 200;
const MAX_MATCHES_PER_FILE: usize = 10;
const MAX_DISPLAY_MATCHES_PER_FILE: usize = 5;

pub fn handle_grep(req: &RawRequest, ctx: &AppContext) -> Response {
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
    let max_results = req
        .params
        .get("max_results")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_MAX_RESULTS);

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
    ) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let project_root = grep_executor::project_root(ctx);
    let total_started = std::time::Instant::now();
    let search_start = std::time::Instant::now();
    let params = GrepParams {
        include,
        exclude,
        max_results,
        path_exclusion: None,
    };
    let (result, phases) =
        grep_executor::execute_profiled_with_filters(ctx, &compiled, &scope, &params, &filters);
    let search_ms = search_start.elapsed().as_secs_f64() * 1000.0;
    let scope_probe_started = std::time::Instant::now();
    let scope_has_files = phases
        .indexed_scope_has_files
        .unwrap_or_else(|| grep_executor::scope_has_files(&project_root, &scope));
    let scope_probe = scope_probe_started.elapsed();
    let format_started = std::time::Instant::now();
    let text = format_grep_text(&result, &project_root);
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
        "complete": !result.walk_truncated && result.skipped_foreign_mounts == 0,
        "no_files_matched_scope": !scope_has_files,
        "skipped_foreign_mounts": result.skipped_foreign_mounts,
        "matches": result.matches.iter().map(match_to_json).collect::<Vec<_>>(),
        "total_matches": result.total_matches,
        "files_searched": result.files_searched,
        "files_with_matches": result.files_with_matches,
        "index_status": result.index_status.as_str(),
        "truncated": result.truncated,
        "search_ms": (search_ms * 1000.0).round() / 1000.0,
    });
    if result.walk_truncated {
        body["walk_truncated"] = serde_json::Value::Bool(true);
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(Fallback directory walk stopped early: file-count or time budget reached; results may be incomplete.)",
            text
        ));
    }
    if result.skipped_foreign_mounts > 0 {
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(Fallback directory walk skipped {} foreign filesystem mount(s); results may be incomplete.)",
            body["text"].as_str().unwrap_or_default(),
            result.skipped_foreign_mounts
        ));
    }

    // Disclose the trigram index state and whether this answer came from a
    // filesystem walk instead of the index. A missing or building index never
    // turns into "no matches": the walk above still searched every file.
    body["index"] = trigram_index_disclosure(ctx);
    body["fallback"] = if result.index_status == IndexStatus::Ready {
        serde_json::Value::Null
    } else {
        serde_json::Value::String("filesystem".to_string())
    };

    if let Some(envelope) = crate::list_surfaces::grep::build_grep_envelope(&body) {
        body["matches_list_envelope"] = serde_json::to_value(&envelope).unwrap_or_default();
    }

    let service_ms = total_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    match result.index_status {
        crate::search_index::IndexStatus::Ready => {
            let status = if result.truncated || result.walk_truncated {
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
    let footer = match result.index_status {
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
    };

    if sections.is_empty() {
        footer
    } else {
        format!("{}\n\n{}", sections.join("\n\n"), footer)
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
