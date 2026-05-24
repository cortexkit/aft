//! LSP-enhanced symbol disambiguation.
//!
//! When the plugin has LSP access, it can attach `lsp_hints` to a request with
//! file + line information for the symbol(s) in play. This module parses those
//! hints and uses them to narrow ambiguous tree-sitter matches down to the
//! single correct candidate.

use crate::protocol::RawRequest;
use crate::symbols::SymbolMatch;
use serde::Deserialize;
use std::path::Path;

/// A single LSP-sourced symbol hint: name, file path, line number, and optional kind.
#[derive(Debug, Clone, Deserialize)]
pub struct LspSymbolHint {
    pub name: String,
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Collection of LSP symbol hints attached to a request.
#[derive(Debug, Clone, Deserialize)]
pub struct LspHints {
    pub symbols: Vec<LspSymbolHint>,
}

/// Strip `file://` URI prefix from a path, returning the bare filesystem path.
fn strip_file_uri(path: &str) -> &str {
    path.strip_prefix("file://").unwrap_or(path)
}

/// Parse `lsp_hints` from a raw request.
///
/// Returns `Some(hints)` if `req.lsp_hints` is present and valid JSON matching
/// the `LspHints` schema. Returns `None` (with a stderr warning) on malformed
/// data, and `None` silently when the field is absent.
pub fn parse_lsp_hints(req: &RawRequest) -> Option<LspHints> {
    let value = req.lsp_hints.as_ref()?;
    match serde_json::from_value::<LspHints>(value.clone()) {
        Ok(hints) => {
            log::debug!("lsp_hints: parsed {} symbol hints", hints.symbols.len());
            Some(hints)
        }
        Err(e) => {
            crate::slog_warn!("lsp_hints: ignoring malformed data: {}", e);
            None
        }
    }
}

/// Use LSP hints to disambiguate multiple tree-sitter symbol matches.
///
/// For each candidate match, checks whether any hint's name + file + line aligns
/// (the hint line falls within the symbol's start_line..=end_line range). If
/// exactly one candidate aligns with a hint, returns just that match. Otherwise,
/// returns all matches unchanged (graceful fallback).
pub fn apply_lsp_disambiguation(matches: Vec<SymbolMatch>, hints: &LspHints) -> Vec<SymbolMatch> {
    if matches.len() <= 1 || hints.symbols.is_empty() {
        return matches;
    }

    let aligned_indices: Vec<usize> = matches
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let is_aligned = hints.symbols.iter().any(|hint| {
                let hint_file = strip_file_uri(&hint.file);
                hint.name == m.symbol.name
                    && paths_match(hint_file, &m.file)
                    && hint.line >= m.symbol.range.start_line
                    && hint.line <= m.symbol.range.end_line
            });
            if is_aligned {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    // Only disambiguate if we narrowed to exactly one match.
    // If zero or multiple still match, fall back to all original candidates.
    if aligned_indices.len() == 1 {
        let idx = aligned_indices[0];
        matches
            .into_iter()
            .nth(idx)
            .map_or_else(Vec::new, |m| vec![m])
    } else {
        matches
    }
}

/// Check if two file paths refer to the same file.
/// Compares canonical paths when possible, then falls back to separator-bounded suffix matching.
fn paths_match(hint_path: &str, match_path: &str) -> bool {
    if let (Ok(hint), Ok(m)) = (
        std::fs::canonicalize(Path::new(hint_path)),
        std::fs::canonicalize(Path::new(match_path)),
    ) {
        return hint == m;
    }

    let hint = hint_path.replace('\\', "/");
    let m = match_path.replace('\\', "/");

    if hint == m {
        return true;
    }

    if hint.len() >= m.len() {
        suffix_at_separator_boundary(&hint, &m)
    } else {
        suffix_at_separator_boundary(&m, &hint)
    }
}

fn suffix_at_separator_boundary(longer: &str, shorter: &str) -> bool {
    if shorter.is_empty() || longer.len() <= shorter.len() || !longer.ends_with(shorter) {
        return false;
    }

    longer.as_bytes()[longer.len() - shorter.len() - 1] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};

    fn make_request(lsp_hints: Option<serde_json::Value>) -> RawRequest {
        RawRequest {
            id: "test-1".into(),
            command: "edit_symbol".into(),
            lsp_hints,
            session_id: None,
            params: serde_json::json!({}),
        }
    }

    fn make_match(
        name: &str,
        file: &str,
        start_line: u32,
        end_line: u32,
        kind: SymbolKind,
    ) -> SymbolMatch {
        SymbolMatch {
            symbol: Symbol {
                name: name.into(),
                kind,
                range: Range {
                    start_line,
                    start_col: 0,
                    end_line,
                    end_col: 0,
                },
                signature: None,
                scope_chain: vec![],
                exported: true,
                parent: None,
            },
            file: file.into(),
        }
    }

    // --- Parsing tests ---

    #[test]
    fn parse_valid_hints() {
        let req = make_request(Some(serde_json::json!({
            "symbols": [
                {"name": "process", "file": "src/app.ts", "line": 10, "kind": "function"},
                {"name": "process", "file": "src/app.ts", "line": 25}
            ]
        })));
        let hints = parse_lsp_hints(&req).unwrap();
        assert_eq!(hints.symbols.len(), 2);
        assert_eq!(hints.symbols[0].name, "process");
        assert_eq!(hints.symbols[0].kind, Some("function".into()));
        assert_eq!(hints.symbols[1].kind, None);
    }

    #[test]
    fn parse_absent_hints_returns_none() {
        let req = make_request(None);
        assert!(parse_lsp_hints(&req).is_none());
    }

    #[test]
    fn parse_malformed_json_returns_none() {
        // Missing required "symbols" field
        let req = make_request(Some(serde_json::json!({"bad": "data"})));
        assert!(parse_lsp_hints(&req).is_none());
    }

    #[test]
    fn parse_empty_symbols_array() {
        let req = make_request(Some(serde_json::json!({"symbols": []})));
        let hints = parse_lsp_hints(&req).unwrap();
        assert!(hints.symbols.is_empty());
    }

    #[test]
    fn parse_missing_required_field_in_hint() {
        // Each hint requires name, file, line — missing "line" here
        let req = make_request(Some(serde_json::json!({
            "symbols": [{"name": "foo", "file": "bar.ts"}]
        })));
        assert!(parse_lsp_hints(&req).is_none());
    }

    // --- Disambiguation tests ---

    #[test]
    fn disambiguate_single_match_by_line() {
        let matches = vec![
            make_match("process", "src/app.ts", 2, 4, SymbolKind::Function),
            make_match("process", "src/app.ts", 7, 10, SymbolKind::Method),
        ];
        let hints = LspHints {
            symbols: vec![LspSymbolHint {
                name: "process".into(),
                file: "src/app.ts".into(),
                line: 3,
                kind: None,
            }],
        };
        let result = apply_lsp_disambiguation(matches, &hints);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol.range.start_line, 2);
    }

    #[test]
    fn disambiguate_no_match_returns_all() {
        let matches = vec![
            make_match("process", "src/app.ts", 2, 4, SymbolKind::Function),
            make_match("process", "src/app.ts", 7, 10, SymbolKind::Method),
        ];
        let hints = LspHints {
            symbols: vec![LspSymbolHint {
                name: "process".into(),
                file: "other/file.ts".into(),
                line: 99,
                kind: None,
            }],
        };
        let result = apply_lsp_disambiguation(matches, &hints);
        assert_eq!(
            result.len(),
            2,
            "no hint matches → fallback to all candidates"
        );
    }

    #[test]
    fn disambiguate_stale_hint_ignored() {
        // Hint line doesn't fall in any symbol's range
        let matches = vec![
            make_match("process", "src/app.ts", 2, 4, SymbolKind::Function),
            make_match("process", "src/app.ts", 7, 10, SymbolKind::Method),
        ];
        let hints = LspHints {
            symbols: vec![LspSymbolHint {
                name: "process".into(),
                file: "src/app.ts".into(),
                line: 50, // stale — doesn't match either range
                kind: None,
            }],
        };
        let result = apply_lsp_disambiguation(matches, &hints);
        assert_eq!(
            result.len(),
            2,
            "stale hint should fall back to all candidates"
        );
    }

    #[test]
    fn disambiguate_file_uri_stripped() {
        let matches = vec![
            make_match("handler", "src/api.ts", 10, 20, SymbolKind::Function),
            make_match("handler", "src/api.ts", 30, 40, SymbolKind::Function),
        ];
        let hints = LspHints {
            symbols: vec![LspSymbolHint {
                name: "handler".into(),
                file: "file://src/api.ts".into(),
                line: 15,
                kind: None,
            }],
        };
        let result = apply_lsp_disambiguation(matches, &hints);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol.range.start_line, 10);
    }

    #[test]
    fn disambiguate_single_input_unchanged() {
        let matches = vec![make_match("foo", "bar.ts", 1, 5, SymbolKind::Function)];
        let hints = LspHints {
            symbols: vec![LspSymbolHint {
                name: "foo".into(),
                file: "bar.ts".into(),
                line: 3,
                kind: None,
            }],
        };
        let result = apply_lsp_disambiguation(matches, &hints);
        assert_eq!(result.len(), 1);
    }

    // --- Path matching tests ---

    #[test]
    fn paths_match_exact() {
        assert!(paths_match("src/app.ts", "src/app.ts"));
    }

    #[test]
    fn paths_match_suffix() {
        assert!(paths_match("/home/user/project/src/app.ts", "src/app.ts"));
    }

    #[test]
    fn paths_match_filename_suffix_at_separator_boundary() {
        assert!(paths_match("/home/user/project/src/app.ts", "app.ts"));
    }

    #[test]
    fn paths_do_not_match_partial_filename_suffixes() {
        assert!(!paths_match("foo/bar/baz.ts", "z.ts"));
        assert!(!paths_match("foo/bar.ts", "ar.ts"));
    }

    #[test]
    fn paths_no_match() {
        assert!(!paths_match("src/other.ts", "src/app.ts"));
    }
}
