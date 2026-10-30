//! Handler for the `ast_search` command: AST-aware pattern search using ast-grep.

use std::collections::HashMap;
use std::path::Path;

use ast_grep_core::tree_sitter::LanguageExt;

use crate::ast_grep_hints::detect_pattern_hint;
use crate::ast_grep_lang::AstGrepLang;
use crate::commands::ast_scope::collect_ast_files;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle an `ast_search` request.
///
/// Params:
///   - `pattern` (string, required) — ast-grep pattern (e.g. `console.log($MSG)`)
///   - `lang` (string, required) — selects which language parser ast-grep should use for matching; accepted values are typescript, tsx, javascript, python, rust, go, c, cpp, cuda, metal, zig, csharp, solidity, vue, pascal, r, groovy, objc
///   - `paths` (string[], optional) — directories/files to search (default: project root)
///   - `globs` (string[], optional) — include/exclude glob filters; prefix `!` to exclude
///   - `context` (integer, optional) — lines of context around each match (default: 0)
///
/// Returns: `{ matches: [{ file, line, column, text, meta_variables, meta_variable_text?, context? }], total_matches, files_with_matches, files_searched, complete, skipped_files }`
///
/// `meta_variable_text` maps each non-empty `$$$` capture to its source text.
pub fn handle_ast_search(req: &RawRequest, ctx: &AppContext) -> Response {
    let pattern = match req.params.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "ast_search: missing required param 'pattern'",
            );
        }
    };

    let lang_str = match req.params.get("lang").and_then(|v| v.as_str()) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "ast_search: missing required param 'lang'",
            );
        }
    };

    let lang = match AstGrepLang::from_str(lang_str) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "ast_search: unsupported language '{lang_str}'. Supported: typescript, tsx, javascript, python, rust, go, c, cpp, cuda, metal, zig, csharp, solidity, vue, pascal, r, groovy, objc"
                ),
            );
        }
    };

    let paths: Vec<String> = req
        .params
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let globs: Vec<String> = req
        .params
        .get("globs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let context_lines = req
        .params
        .get("context")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    // Validate the pattern before searching. ast-grep-core can panic (via unwrap) on
    // patterns that parse to multiple AST nodes (e.g. bare `catch` or `finally`
    // clauses). Release builds use panic="unwind", so catch_unwind is effective,
    // but returning an explicit pattern error gives callers a better signal.
    // The compiled pattern is kept and reused for every file below.
    #[cfg(test)]
    PATTERN_COMPILES.with(|count| count.set(count.get() + 1));
    let compiled_pattern = match lang.compile_pattern(&pattern) {
        Ok(compiled) => compiled,
        Err(err) => {
            // Attach a hint when the pattern looks like a common mistake. The
            // hint helps less-capable agents recover from regex-shaped patterns
            // and language-specific shape gotchas.
            let mut message = format!("invalid AST pattern: {}", err);
            if let Some(hint) = detect_pattern_hint(&pattern, &lang) {
                message.push_str("\n\n");
                message.push_str(&hint);
            }
            return Response::error(&req.id, "invalid_pattern", message);
        }
    };

    let config = ctx.config();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    drop(config);

    let scope = match collect_ast_files(
        &req.id,
        Some(req.session()),
        "ast_search",
        ctx,
        &project_root,
        &lang,
        &paths,
        &globs,
    ) {
        Ok(scope) => scope,
        Err(resp) => return resp,
    };

    use rayon::prelude::*;
    let bounded = crate::subc::result_body::active();
    let mut budget = MatchBudget {
        remaining: bounded.then_some(256 * 1024),
        cut: false,
    };
    let search = |file_path: &std::path::PathBuf, budget: &mut MatchBudget| {
        let source = match crate::bounded_io::read_to_string(file_path) {
            Ok(s) => s,
            Err(err) => {
                return (
                    0usize,
                    0usize,
                    Vec::new(),
                    Some(serde_json::json!({
                        "file": file_path.display().to_string(),
                        "reason": format!("read_error: {err}"),
                    })),
                );
            }
        };
        let (total, matches) = search_file_compiled(
            &source,
            file_path,
            &compiled_pattern,
            &lang,
            context_lines,
            budget,
        );
        (1usize, total, matches, None)
    };
    // A single producer owns the opted-in byte budget, retaining a deterministic
    // prefix while still counting every match. Legacy searches keep their parallel path.
    let per_file: Vec<_> = if bounded {
        scope
            .files
            .iter()
            .map(|path| search(path, &mut budget))
            .collect()
    } else {
        scope
            .files
            .par_iter()
            .map(|path| search(path, &mut MatchBudget::default()))
            .collect()
    };

    let mut all_matches: Vec<serde_json::Value> = Vec::new();
    let mut skipped_files: Vec<serde_json::Value> = Vec::new();
    let mut files_searched: usize = 0;
    let mut files_with_matches: usize = 0;
    let mut total_matches = 0;
    for (counted, total, matches, skipped) in per_file {
        files_searched += counted;
        total_matches += total;
        if total > 0 {
            files_with_matches += 1;
        }
        all_matches.extend(matches);
        if let Some(skipped) = skipped {
            skipped_files.push(skipped);
        }
    }
    skipped_files.sort_by(|a, b| {
        let a_file = a.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let b_file = b.get("file").and_then(|v| v.as_str()).unwrap_or("");
        a_file.cmp(b_file)
    });

    let shown = all_matches.len();
    let complete = skipped_files.is_empty() && !budget.cut;

    let mut payload = serde_json::json!({
        "matches": all_matches,
        "total_matches": total_matches,
        "files_with_matches": files_with_matches,
        "files_searched": files_searched,
        "no_files_matched_scope": scope.no_files_matched_scope,
        "scope_warnings": scope.scope_warnings,
        "complete": complete,
        "skipped_files": skipped_files,
    });

    if budget.cut {
        payload["truncated"] = serde_json::json!(true);
        payload["matches_list_envelope"] =
            serde_json::to_value(crate::list_surfaces::ast_search_envelope(
                shown,
                total_matches,
                !payload["skipped_files"]
                    .as_array()
                    .is_some_and(|files| files.is_empty()),
            ))
            .expect("list envelope is JSON");
    }

    // When the search succeeded but matched zero AST nodes, attach a hint if
    // the pattern looks like a common mistake (regex syntax, language-specific
    // shape gotcha, today's Rust match-arm `|` trap, etc.). Agents reading
    // `total_matches: 0` as "no work to do" can now see why.
    if total_matches == 0 && !scope.no_files_matched_scope {
        if let Some(hint) = detect_pattern_hint(&pattern, &lang) {
            payload["hint"] = serde_json::Value::String(hint);
        }
    }

    Response::success(&req.id, payload)
}

#[derive(Default)]
struct MatchBudget {
    remaining: Option<usize>,
    cut: bool,
}

fn search_file_compiled(
    source: &str,
    file_path: &Path,
    pattern: &ast_grep_core::matcher::Pattern,
    lang: &AstGrepLang,
    context_lines: usize,
    budget: &mut MatchBudget,
) -> (usize, Vec<serde_json::Value>) {
    let ast_grep = lang.ast_grep(source);
    let root = ast_grep.root();

    let source_lines: Vec<&str> = source.lines().collect();
    let file_str = file_path.display().to_string();

    let mut total = 0;
    let matches = root
        .find_all(pattern)
        .filter_map(|node_match| {
            total += 1;
            if budget.remaining == Some(0) {
                budget.cut = true;
                return None;
            }
            let start_pos = node_match.start_pos();
            let line_1based = start_pos.line() + 1;
            let column = start_pos.byte_point().1;
            let text = node_match.text().to_string();

            let env = node_match.get_env();
            let mut meta_vars: HashMap<String, serde_json::Value> = HashMap::new();
            // Source text of each `$$$` capture, from its first node to its
            // last, separators and line breaks included. The array in
            // `meta_variables` lists every matched node, punctuation included,
            // so joining it does not reproduce the source.
            let mut multi_capture_text: HashMap<String, serde_json::Value> = HashMap::new();
            for meta_var in env.get_matched_variables() {
                use ast_grep_core::meta_var::MetaVariable;
                match &meta_var {
                    MetaVariable::Capture(name, _) => {
                        if let Some(node) = env.get_match(name) {
                            meta_vars.insert(
                                format!("${}", name),
                                serde_json::Value::String(node.text().to_string()),
                            );
                        }
                    }
                    MetaVariable::MultiCapture(name) => {
                        let nodes = env.get_multiple_matches(name);
                        if let (Some(first), Some(last)) = (nodes.first(), nodes.last()) {
                            if let Some(text) = source.get(first.range().start..last.range().end) {
                                multi_capture_text.insert(
                                    format!("${}", name),
                                    serde_json::Value::String(text.to_string()),
                                );
                            }
                        }
                        let texts: Vec<serde_json::Value> = nodes
                            .iter()
                            .map(|n| serde_json::Value::String(n.text().to_string()))
                            .collect();
                        meta_vars.insert(format!("${}", name), serde_json::Value::Array(texts));
                    }
                    _ => {}
                }
            }

            let mut result = serde_json::json!({
                "file": file_str,
                "line": line_1based,
                "column": column,
                "text": text,
                "meta_variables": meta_vars,
            });
            if !multi_capture_text.is_empty() {
                result["meta_variable_text"] = serde_json::json!(multi_capture_text);
            }

            if context_lines > 0 {
                let match_line_0 = start_pos.line();
                let end_line_0 = node_match.end_pos().line();
                let ctx_start = match_line_0.saturating_sub(context_lines);
                let ctx_end = (end_line_0 + context_lines + 1).min(source_lines.len());
                let context: Vec<serde_json::Value> = (ctx_start..ctx_end)
                    .map(|i| {
                        serde_json::json!({
                            "line": i + 1,
                            "text": source_lines[i],
                            "is_match": i >= match_line_0 && i <= end_line_0,
                        })
                    })
                    .collect();
                result["context"] = serde_json::Value::Array(context);
            }

            if let Some(remaining) = &mut budget.remaining {
                let bytes = serde_json::to_vec(&result).expect("match is JSON").len();
                if bytes > *remaining {
                    *remaining = 0;
                    budget.cut = true;
                    return None;
                }
                *remaining -= bytes;
            }
            Some(result)
        })
        .collect();
    (total, matches)
}

#[cfg(test)]
thread_local! {
    /// Pattern compilations `handle_ast_search` did on this thread.
    static PATTERN_COMPILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pattern is validated and searched with one compilation; the
    /// handler used to compile it a second time after the scope walk.
    #[test]
    fn ast_search_compiles_the_pattern_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("a.ts"), "console.log(1);\nconsole.log(2);\n").unwrap();
        let config = crate::config::Config {
            project_root: Some(root.clone()),
            ..crate::config::Config::default()
        };
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "search",
            "command": "ast_search",
            "pattern": "console.log($X)",
            "lang": "typescript",
        }))
        .unwrap();
        let before = PATTERN_COMPILES.with(|count| count.get());
        let response = serde_json::to_value(handle_ast_search(&request, &ctx)).unwrap();
        assert_eq!(PATTERN_COMPILES.with(|count| count.get()) - before, 1);
        assert_eq!(response["total_matches"], 2, "{response}");
    }
}
