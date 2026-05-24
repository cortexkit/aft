---
estimated_steps: 5
estimated_files: 5
---

# T02: Zoom command with caller/callee annotations

**Slice:** S03 — Structural Reading
**Milestone:** M001

## Description

Implement the `zoom` command — resolves a symbol by name, extracts its source body with surrounding context lines, and annotates with file-scoped caller/callee information. This is the more complex S03 command: it requires symbol resolution (with disambiguation), source line extraction (0-based ranges), and AST walking for call expression discovery across all 6 supported languages.

Caller/callee analysis is file-scoped only (cross-file is M003/R020). Call expressions are discovered by walking tree-sitter AST nodes within symbol byte ranges. Language-specific node kinds: TS/JS/Go use `call_expression`, Python uses `call`, Rust uses `call_expression` + `macro_invocation`. Member access calls (`obj.method()`) use a last-segment heuristic to extract the method name for matching against known symbols.

## Steps

1. Create `tests/fixtures/calls.ts` — TS fixture with intra-file function calls: multiple functions that call each other, member access calls, unused functions. This gives us concrete call graph edges to verify.
2. Create `src/commands/zoom.rs` — define `ZoomParams` (file, symbol, optional context_lines), `ZoomResponse` (name, kind, range, content, context_before, context_after, annotations: { calls_out, called_by }), `CallRef` (name, line). Implement `handle_zoom`:
   - Call `resolve_symbol(file, name)` — if 0 matches → SymbolNotFound error, if >1 match → AmbiguousSymbol error with qualified candidates, if exactly 1 → proceed
   - Read source file, split into lines (0-based indexing)
   - Extract symbol body: `source_lines[range.start_line..=range.end_line]`
   - Extract context: `context_before` = lines before start (clamped to 0), `context_after` = lines after end (clamped to file length)
   - Walk AST within symbol's byte range for call expression nodes → extract callee names → match against file's known symbols → build `calls_out`
   - Walk AST within all OTHER symbols' byte ranges for calls to this symbol → build `called_by`
   - Return Response::success with ZoomResponse
3. Implement call extraction helpers: `extract_calls_in_range(source, tree, byte_start, byte_end, lang) -> Vec<String>` that walks the AST subtree within the given byte range, finds call_expression/call/macro_invocation nodes, extracts the function name (last segment for member access). Keep language-specific node kind mapping simple — a match on LangId.
4. Wire `"zoom"` into main.rs dispatch. Update `src/commands/mod.rs` to export zoom handler.
5. Add unit tests (call extraction logic, context line clamping, body extraction) and integration tests (zoom via binary protocol — successful zoom, ambiguous symbol error, symbol not found error, context_lines parameter).

## Must-Haves

- [ ] Zoom returns correct symbol body content (source lines within range)
- [ ] context_before and context_after respect file boundaries (no panic on first/last function)
- [ ] calls_out lists functions called within the zoomed symbol's body
- [ ] called_by lists functions that call the zoomed symbol (file-scoped)
- [ ] Member access calls (`obj.method()`) extract last segment for symbol matching
- [ ] Ambiguous symbol name returns AmbiguousSymbol error with candidates including scope chains
- [ ] Missing symbol returns SymbolNotFound error
- [ ] Empty calls_out/called_by returned as empty arrays (not null/missing)
- [ ] Rust macro invocations (`println!()`) included in calls_out
- [ ] Optional `context_lines` parameter works (default 3)

## Verification

- `cargo test --lib -- commands::zoom::tests` — all zoom unit tests pass
- `cargo test --test integration -- test_zoom` — integration tests pass (success case, ambiguous error, not-found error)
- `cargo build` — 0 errors, 0 warnings
- All existing tests still pass: `cargo test`

## Observability Impact

- Signals added/changed: zoom error responses use existing AftError codes (`symbol_not_found`, `ambiguous_symbol`, `file_not_found`, `invalid_request`)
- How a future agent inspects this: send zoom command with a known symbol name, check response has `content`, `annotations.calls_out`, `annotations.called_by` fields
- Failure state exposed: ambiguous_symbol error includes candidates array with qualified names for disambiguation

## Inputs

- `src/commands/mod.rs` — module structure from T01
- `src/commands/outline.rs` — established pattern for command handler + response types
- `src/main.rs` — dispatch with provider wiring from T01
- `src/parser.rs` — `TreeSitterProvider.resolve_symbol()` for symbol lookup, `FileParser.parse()` for direct AST access, `node_text()` and `node_range()` helpers
- `src/symbols.rs` — `Symbol` struct with range, `SymbolMatch` with file path
- `src/language.rs` — `LanguageProvider` trait
- `tests/integration/commands_test.rs` — integration test file from T01

## Expected Output

- `src/commands/zoom.rs` — zoom handler with call extraction, context lines, disambiguation, unit tests
- `src/commands/mod.rs` — updated with zoom export
- `src/main.rs` — zoom command arm added to dispatch
- `tests/fixtures/calls.ts` — fixture with intra-file function calls for zoom testing
- `tests/integration/commands_test.rs` — zoom integration tests added
