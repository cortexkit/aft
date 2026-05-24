use std::path::Path;

use serde::Serialize;

use crate::context::AppContext;
use crate::edit::line_col_to_byte;
use crate::lsp_hints;
use crate::parser::{detect_language, FileParser, LangId};
use crate::protocol::{RawRequest, Response};
use crate::symbols::Range;

/// A reference to a called/calling function.
#[derive(Debug, Clone, Serialize)]
pub struct CallRef {
    pub name: String,
    /// 1-based line number of the call reference.
    pub line: u32,
}

/// Annotations describing file-scoped call relationships.
#[derive(Debug, Clone, Serialize)]
pub struct Annotations {
    pub calls_out: Vec<CallRef>,
    pub called_by: Vec<CallRef>,
}

/// Response payload for the zoom command.
#[derive(Debug, Clone, Serialize)]
pub struct ZoomResponse {
    pub name: String,
    pub kind: String,
    pub range: Range,
    pub content: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
    pub annotations: Annotations,
}

struct RawCall {
    name: String,
    line: u32,
    start_byte: usize,
    end_byte: usize,
}

/// Handle a `zoom` request.
///
/// Expects `file`, `symbol` in request params, optional `context_lines` (default 3).
/// Resolves the symbol, extracts body + context, walks AST for call annotations.
pub fn handle_zoom(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "zoom: missing required param 'file'",
            );
        }
    };

    let context_lines = req
        .params
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as usize;

    let start_line = req
        .params
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let end_line = req
        .params
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    // Read source file early because both symbol mode and line-range mode need it.
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, e));
        }
    };

    let lines: Vec<String> = source.lines().map(|l| l.to_string()).collect();

    // Line-range mode: read arbitrary lines without requiring a symbol.
    match (start_line, end_line) {
        (Some(start), Some(end)) => {
            if req.params.get("symbol").is_some() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "zoom: provide either 'symbol' OR ('start_line' and 'end_line'), not both",
                );
            }
            if start == 0 || end == 0 {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "zoom: 'start_line' and 'end_line' are 1-based and must be >= 1",
                );
            }
            if end < start {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("zoom: end_line {} must be >= start_line {}", end, start),
                );
            }
            if lines.is_empty() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("zoom: {} is empty", file),
                );
            }

            let start_idx = start - 1;
            // Clamp end_line to file length (same as batch edits)
            let clamped_end = end.min(lines.len());
            let end_idx = clamped_end - 1;
            if start_idx >= lines.len() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "zoom: start_line {} is past end of {} ({} lines)",
                        start,
                        file,
                        lines.len()
                    ),
                );
            }

            let content = lines[start_idx..=end_idx].join("\n");
            let ctx_start = start_idx.saturating_sub(context_lines);
            let context_before: Vec<String> = if ctx_start < start_idx {
                lines[ctx_start..start_idx]
                    .iter()
                    .map(|l| l.to_string())
                    .collect()
            } else {
                vec![]
            };
            let ctx_end = (end_idx + 1 + context_lines).min(lines.len());
            let context_after: Vec<String> = if end_idx + 1 < lines.len() {
                lines[(end_idx + 1)..ctx_end]
                    .iter()
                    .map(|l| l.to_string())
                    .collect()
            } else {
                vec![]
            };
            let end_col = lines[end_idx].chars().count() as u32;

            return Response::success(
                &req.id,
                serde_json::json!({
                    "name": format!("lines {}-{}", start, clamped_end),
                    "kind": "lines",
                    "range": {
                        "start_line": start,  // already 1-based from user input
                        "start_col": 1,
                        "end_line": clamped_end,
                        "end_col": end_col + 1,
                    },
                    "content": content,
                    "context_before": context_before,
                    "context_after": context_after,
                    "annotations": {
                        "calls_out": [],
                        "called_by": [],
                    },
                }),
            );
        }
        (Some(_), None) | (None, Some(_)) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "zoom: provide both 'start_line' and 'end_line' for line-range mode",
            );
        }
        (None, None) => {}
    }

    let symbol_name = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "zoom: missing required param 'symbol' (or use 'start_line' and 'end_line')",
            );
        }
    };

    // Resolve the target symbol. Markdown/HTML headings are often copied from outline output
    // with a visible level prefix (e.g. "## Basic usage" or "<h2>Features"); normalize only
    // that heading lookup path so code-symbol resolution keeps exact matching semantics.
    let lookup_name = match detect_language(&path) {
        Some(LangId::Markdown | LangId::Html) => normalize_heading_query(symbol_name),
        _ => symbol_name,
    };
    let matches = match ctx.provider().resolve_symbol(&path, lookup_name) {
        Ok(m) => m,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // LSP-enhanced disambiguation (S03)
    let matches = if let Some(hints) = lsp_hints::parse_lsp_hints(req) {
        lsp_hints::apply_lsp_disambiguation(matches, &hints)
    } else {
        matches
    };

    if matches.len() > 1 {
        // Ambiguous — return qualified candidates with 1-based line ranges.
        // Internal symbols.rs ranges are 0-based; we add 1 to both start and end.
        let candidates: Vec<String> = matches
            .iter()
            .map(|m| {
                let sym = &m.symbol;
                let start = sym.range.start_line + 1;
                let end = sym.range.end_line + 1;
                let line_range = if start == end {
                    format!("{}", start)
                } else {
                    format!("{}-{}", start, end)
                };
                if sym.scope_chain.is_empty() {
                    format!("{}:{}", sym.name, line_range)
                } else {
                    format!(
                        "{}::{}:{}",
                        sym.scope_chain.join("::"),
                        sym.name,
                        line_range
                    )
                }
            })
            .collect();
        return Response::error(
            &req.id,
            "ambiguous_symbol",
            format!(
                "symbol '{}' is ambiguous, candidates: [{}]",
                symbol_name,
                candidates.join(", ")
            ),
        );
    }

    let target = &matches[0].symbol;
    let start = target.range.start_line as usize;
    let end = target.range.end_line as usize;

    // When re-export following resolved to a different file, re-read that file's lines
    let resolved_file_path = std::path::Path::new(&matches[0].file);
    let resolved_lines: Vec<String>;
    let effective_lines: &[String] = if resolved_file_path != path {
        resolved_lines = match std::fs::read_to_string(resolved_file_path) {
            Ok(src) => src.lines().map(|l| l.to_string()).collect(),
            Err(_) => lines.clone(),
        };
        &resolved_lines
    } else {
        &lines
    };

    // Extract symbol body (0-based line indices)
    let content = if end < effective_lines.len() {
        effective_lines[start..=end].join("\n")
    } else {
        effective_lines[start..].join("\n")
    };

    // Context before
    let ctx_start = start.saturating_sub(context_lines);
    let context_before: Vec<String> = if ctx_start < start {
        effective_lines[ctx_start..start]
            .iter()
            .map(|l| l.to_string())
            .collect()
    } else {
        vec![]
    };

    // Context after
    let ctx_end = (end + 1 + context_lines).min(effective_lines.len());
    let context_after: Vec<String> = if end + 1 < effective_lines.len() {
        effective_lines[(end + 1)..ctx_end]
            .iter()
            .map(|l| l.to_string())
            .collect()
    } else {
        vec![]
    };

    // Get all symbols in the resolved file for call matching
    let all_symbols = match ctx.provider().list_symbols(resolved_file_path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    let known_names: Vec<&str> = all_symbols.iter().map(|s| s.name.as_str()).collect();

    // Parse AST for call extraction (use resolved file for cross-file re-exports)
    let mut parser = FileParser::with_symbol_cache(ctx.symbol_cache());
    let (tree, lang) = match parser.parse(resolved_file_path) {
        Ok(r) => r,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // calls_out: calls within the target symbol's byte range
    let resolved_source = if resolved_file_path != path {
        std::fs::read_to_string(resolved_file_path).unwrap_or_else(|_| source.clone())
    } else {
        source.clone()
    };
    let target_byte_start = line_col_to_byte(
        &resolved_source,
        target.range.start_line,
        target.range.start_col,
    );
    let target_byte_end = line_col_to_byte(
        &resolved_source,
        target.range.end_line,
        target.range.end_col,
    );

    let all_file_calls = extract_calls_with_ranges(&resolved_source, tree.root_node(), lang);

    let raw_calls = all_file_calls
        .iter()
        .filter(|call| call.start_byte >= target_byte_start && call.end_byte <= target_byte_end);
    let calls_out: Vec<CallRef> = raw_calls
        .filter(|call| known_names.contains(&call.name.as_str()) && call.name != target.name)
        .map(|call| CallRef {
            name: call.name.clone(),
            line: call.line,
        })
        .collect();

    // called_by: bucket the single file-wide call extraction by enclosing symbol range
    let mut called_by: Vec<CallRef> = Vec::new();
    for sym in &all_symbols {
        if sym.name == target.name && sym.range.start_line == target.range.start_line {
            continue; // skip self
        }
        let sym_byte_start =
            line_col_to_byte(&resolved_source, sym.range.start_line, sym.range.start_col);
        let sym_byte_end =
            line_col_to_byte(&resolved_source, sym.range.end_line, sym.range.end_col);
        for call in &all_file_calls {
            if call.name == target.name
                && call.start_byte >= sym_byte_start
                && call.end_byte <= sym_byte_end
            {
                called_by.push(CallRef {
                    name: sym.name.clone(),
                    line: call.line,
                });
            }
        }
    }

    // Dedup called_by by (name, line)
    called_by.sort_by(|a, b| a.name.cmp(&b.name).then(a.line.cmp(&b.line)));
    called_by.dedup_by(|a, b| a.name == b.name && a.line == b.line);

    let kind_str = serde_json::to_value(&target.kind)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{:?}", target.kind).to_lowercase());

    let resp = ZoomResponse {
        name: target.name.clone(),
        kind: kind_str,
        range: target.range.clone(),
        content,
        context_before,
        context_after,
        annotations: Annotations {
            calls_out,
            called_by,
        },
    };

    match serde_json::to_value(&resp) {
        Ok(resp_json) => Response::success(&req.id, resp_json),
        Err(err) => Response::error(
            &req.id,
            "internal_error",
            format!("zoom: failed to serialize response: {err}"),
        ),
    }
}

fn normalize_heading_query(input: &str) -> &str {
    let trimmed = input.trim_start();
    let hash_stripped = trimmed.trim_start_matches('#').trim_start();

    if let Some(after_open) = hash_stripped.strip_prefix('<') {
        let after_slash = after_open.strip_prefix('/').unwrap_or(after_open);
        let mut chars = after_slash.chars();
        if matches!(chars.next(), Some('h' | 'H')) && matches!(chars.next(), Some('1'..='6')) {
            if let Some(end) = hash_stripped.find('>') {
                return hash_stripped[end + 1..].trim_start();
            }
        }
    }

    hash_stripped
}

/// Extract call expression names within a byte range of the AST.
///
/// Delegates to `crate::calls::extract_calls_in_range`.
#[cfg(test)]
fn extract_calls_in_range(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> Vec<(String, u32)> {
    crate::calls::extract_calls_in_range(source, root, byte_start, byte_end, lang)
}

fn extract_calls_with_ranges(source: &str, root: tree_sitter::Node, lang: LangId) -> Vec<RawCall> {
    let mut results = Vec::new();
    let call_kinds = crate::calls::call_node_kinds(lang);
    collect_calls_with_ranges(root, source, &call_kinds, &mut results);
    results
}

fn collect_calls_with_ranges(
    node: tree_sitter::Node,
    source: &str,
    call_kinds: &[&str],
    results: &mut Vec<RawCall>,
) {
    if call_kinds.contains(&node.kind()) {
        if let Some(name) = crate::calls::extract_callee_name(&node, source) {
            results.push(RawCall {
                name,
                line: node.start_position().row as u32 + 1,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_calls_with_ranges(cursor.node(), source, call_kinds, results);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn make_ctx() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    // --- Call extraction tests ---

    #[test]
    fn extract_calls_finds_direct_calls() {
        let source = std::fs::read_to_string(fixture_path("calls.ts")).unwrap();
        let mut parser = FileParser::new();
        let path = fixture_path("calls.ts");
        let (tree, lang) = parser.parse(&path).unwrap();

        // `compute` calls `helper` — find compute's range from symbols
        let ctx = make_ctx();
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let compute = symbols.iter().find(|s| s.name == "compute").unwrap();

        let byte_start =
            line_col_to_byte(&source, compute.range.start_line, compute.range.start_col);
        let byte_end = line_col_to_byte(&source, compute.range.end_line, compute.range.end_col);

        let calls = extract_calls_in_range(&source, tree.root_node(), byte_start, byte_end, lang);
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();

        assert!(
            names.contains(&"helper"),
            "compute should call helper, got: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_finds_member_calls() {
        let source = std::fs::read_to_string(fixture_path("calls.ts")).unwrap();
        let mut parser = FileParser::new();
        let path = fixture_path("calls.ts");
        let (tree, lang) = parser.parse(&path).unwrap();

        let ctx = make_ctx();
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let run_all = symbols.iter().find(|s| s.name == "runAll").unwrap();

        let byte_start =
            line_col_to_byte(&source, run_all.range.start_line, run_all.range.start_col);
        let byte_end = line_col_to_byte(&source, run_all.range.end_line, run_all.range.end_col);

        let calls = extract_calls_in_range(&source, tree.root_node(), byte_start, byte_end, lang);
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();

        assert!(
            names.contains(&"add"),
            "runAll should call this.add, got: {:?}",
            names
        );
        assert!(
            names.contains(&"helper"),
            "runAll should call helper, got: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_unused_function_has_no_calls() {
        let source = std::fs::read_to_string(fixture_path("calls.ts")).unwrap();
        let mut parser = FileParser::new();
        let path = fixture_path("calls.ts");
        let (tree, lang) = parser.parse(&path).unwrap();

        let ctx = make_ctx();
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let unused = symbols.iter().find(|s| s.name == "unused").unwrap();

        let byte_start = line_col_to_byte(&source, unused.range.start_line, unused.range.start_col);
        let byte_end = line_col_to_byte(&source, unused.range.end_line, unused.range.end_col);

        let calls = extract_calls_in_range(&source, tree.root_node(), byte_start, byte_end, lang);
        // console.log is the only call, but "log" or "console" aren't known symbols
        let known_names = [
            "helper",
            "compute",
            "orchestrate",
            "unused",
            "format",
            "display",
        ];
        let filtered: Vec<&str> = calls
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| known_names.contains(n))
            .collect();
        assert!(
            filtered.is_empty(),
            "unused should not call known symbols, got: {:?}",
            filtered
        );
    }

    // --- Context line tests ---

    #[test]
    fn context_lines_clamp_at_file_start() {
        // helper() is at the top of the file (line 2) — context_before should be clamped
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let helper = symbols.iter().find(|s| s.name == "helper").unwrap();

        let source = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let start = helper.range.start_line as usize;

        // With context_lines=5, ctx_start should clamp to 0
        let ctx_start = start.saturating_sub(5);
        let context_before: Vec<&str> = lines[ctx_start..start].to_vec();
        // Should have at most `start` lines (not panic)
        assert!(context_before.len() <= start);
    }

    #[test]
    fn context_lines_clamp_at_file_end() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let display = symbols.iter().find(|s| s.name == "display").unwrap();

        let source = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let end = display.range.end_line as usize;

        // With context_lines=20, should clamp to file length
        let ctx_end = (end + 1 + 20).min(lines.len());
        let context_after: Vec<&str> = if end + 1 < lines.len() {
            lines[(end + 1)..ctx_end].to_vec()
        } else {
            vec![]
        };
        // Should not panic regardless of context_lines size
        assert!(context_after.len() <= 20);
    }

    // --- Body extraction test ---

    #[test]
    fn body_extraction_matches_source() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let compute = symbols.iter().find(|s| s.name == "compute").unwrap();

        let source = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let start = compute.range.start_line as usize;
        let end = compute.range.end_line as usize;
        let body = lines[start..=end].join("\n");

        assert!(
            body.contains("function compute"),
            "body should contain function declaration"
        );
        assert!(
            body.contains("helper(a)"),
            "body should contain call to helper"
        );
        assert!(
            body.contains("doubled + b"),
            "body should contain return expression"
        );
    }

    // --- Full zoom response tests ---

    #[test]
    fn zoom_response_has_calls_out_and_called_by() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-1", path.to_str().unwrap(), "compute", None);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true, "zoom should succeed: {:?}", json);

        let calls_out = json["annotations"]["calls_out"]
            .as_array()
            .expect("calls_out array");
        let out_names: Vec<&str> = calls_out
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            out_names.contains(&"helper"),
            "compute calls helper: {:?}",
            out_names
        );

        let called_by = json["annotations"]["called_by"]
            .as_array()
            .expect("called_by array");
        let by_names: Vec<&str> = called_by
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            by_names.contains(&"orchestrate"),
            "orchestrate calls compute: {:?}",
            by_names
        );
    }

    #[test]
    fn zoom_response_empty_annotations_for_unused() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-2", path.to_str().unwrap(), "unused", None);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);

        let _calls_out = json["annotations"]["calls_out"].as_array().unwrap();
        let called_by = json["annotations"]["called_by"].as_array().unwrap();

        // calls_out exists (may contain console.log but no known symbols)
        // called_by should be empty — nobody calls unused
        assert!(
            called_by.is_empty(),
            "unused should not be called by anyone: {:?}",
            called_by
        );
    }

    #[test]
    fn zoom_symbol_not_found() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-3", path.to_str().unwrap(), "nonexistent", None);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "symbol_not_found");
    }

    #[test]
    fn zoom_custom_context_lines() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-4", path.to_str().unwrap(), "compute", Some(1));
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);

        let ctx_before = json["context_before"].as_array().unwrap();
        let ctx_after = json["context_after"].as_array().unwrap();
        // With context_lines=1, we get at most 1 line before and after
        assert!(
            ctx_before.len() <= 1,
            "context_before should be ≤1: {:?}",
            ctx_before
        );
        assert!(
            ctx_after.len() <= 1,
            "context_after should be ≤1: {:?}",
            ctx_after
        );
    }

    #[test]
    fn zoom_missing_file_param() {
        let ctx = make_ctx();
        let req = make_raw_request("z-5", r#"{"id":"z-5","command":"zoom","symbol":"foo"}"#);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
    }

    #[test]
    fn zoom_missing_symbol_param() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        // Build the JSON via serde_json so Windows paths (with backslashes)
        // are escaped correctly. Hand-formatted JSON would treat `C:\path`
        // backslashes as escape sequences and fail to parse.
        let req_value = serde_json::json!({
            "id": "z-6",
            "command": "zoom",
            "file": path.to_string_lossy(),
        });
        let req_str = req_value.to_string();
        let req: RawRequest = serde_json::from_str(&req_str).unwrap();
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
    }

    // --- Helpers ---

    fn make_zoom_request(
        id: &str,
        file: &str,
        symbol: &str,
        context_lines: Option<u64>,
    ) -> RawRequest {
        let mut json = serde_json::json!({
            "id": id,
            "command": "zoom",
            "file": file,
            "symbol": symbol,
        });
        if let Some(cl) = context_lines {
            json["context_lines"] = serde_json::json!(cl);
        }
        serde_json::from_value(json).unwrap()
    }

    fn make_raw_request(_id: &str, json_str: &str) -> RawRequest {
        serde_json::from_str(json_str).unwrap()
    }
}
