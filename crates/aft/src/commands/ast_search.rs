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
///   - `lang` (string, required) — target language: typescript, tsx, javascript, python, rust, go, c, cpp, zig, csharp, solidity, vue
///   - `paths` (string[], optional) — directories/files to search (default: project root)
///   - `globs` (string[], optional) — include/exclude glob filters; prefix `!` to exclude
///   - `context` (integer, optional) — lines of context around each match (default: 0)
///
/// Returns: `{ matches: [{ file, line, column, text, meta_variables, context? }], total_matches, files_with_matches, files_searched }`
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
                    "ast_search: unsupported language '{}'. Supported: typescript, tsx, javascript, python, rust, go, c, cpp, zig, csharp, solidity, vue",
                    lang_str
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
    use ast_grep_core::matcher::Pattern as AstPattern;
    if let Err(err) = AstPattern::try_new(&pattern, lang.clone()) {
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

    let config = ctx.config();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    drop(config);

    let scope = match collect_ast_files(
        &req.id,
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

    // Pre-build the pattern ONCE so we don't re-parse it per file.
    // Validate first (try_new returns Err on patterns that don't form a single AST node).
    let compiled_pattern = match AstPattern::try_new(&pattern, lang.clone()) {
        Ok(p) => p,
        Err(_) => {
            // Pattern is invalid for this language — return empty result, not error
            return Response::success(
                &req.id,
                serde_json::json!({
                    "matches": [],
                    "total_matches": 0,
                    "files_with_matches": 0,
                    "files_searched": 0,
                    "no_files_matched_scope": scope.no_files_matched_scope,
                    "scope_warnings": scope.scope_warnings,
                }),
            );
        }
    };

    use rayon::prelude::*;
    let per_file: Vec<(usize, Vec<serde_json::Value>)> = scope
        .files
        .par_iter()
        .map(|file_path| {
            let source = match std::fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(_) => return (0usize, Vec::new()),
            };
            let matches =
                search_file_compiled(&source, file_path, &compiled_pattern, &lang, context_lines);
            (1usize, matches)
        })
        .collect();

    let mut all_matches: Vec<serde_json::Value> = Vec::new();
    let mut files_searched: usize = 0;
    let mut files_with_matches: usize = 0;
    for (counted, matches) in per_file {
        files_searched += counted;
        if !matches.is_empty() {
            files_with_matches += 1;
        }
        all_matches.extend(matches);
    }

    let total_matches = all_matches.len();

    let mut payload = serde_json::json!({
        "matches": all_matches,
        "total_matches": total_matches,
        "files_with_matches": files_with_matches,
        "files_searched": files_searched,
        "no_files_matched_scope": scope.no_files_matched_scope,
        "scope_warnings": scope.scope_warnings,
    });

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

fn search_file_compiled(
    source: &str,
    file_path: &Path,
    pattern: &ast_grep_core::matcher::Pattern,
    lang: &AstGrepLang,
    context_lines: usize,
) -> Vec<serde_json::Value> {
    let ast_grep = lang.ast_grep(source);
    let root = ast_grep.root();

    let source_lines: Vec<&str> = source.lines().collect();
    let file_str = file_path.display().to_string();

    let matches_iter: Vec<_> = root.find_all(pattern).collect();

    matches_iter
        .into_iter()
        .map(|node_match| {
            let start_pos = node_match.start_pos();
            let line_1based = start_pos.line() + 1;
            let column = start_pos.byte_point().1;
            let text = node_match.text().to_string();

            let env = node_match.get_env();
            let mut meta_vars: HashMap<String, serde_json::Value> = HashMap::new();
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

            result
        })
        .collect()
}
