---
estimated_steps: 7
estimated_files: 8
---

# T01: Binary-side lsp_hints consumption and disambiguation

**Slice:** S03 — LSP-Enhanced Symbol Resolution
**Milestone:** M004

## Description

Define the `LspHints` data structure in the binary, add defensive parsing from `req.lsp_hints`, implement a shared disambiguation function that filters tree-sitter matches using LSP file+line hints, and wire it into all 4 command handlers that use `resolve_symbol`. Add unit tests for the parsing/disambiguation logic and integration tests proving the protocol-level behavior (hints resolve ambiguity, absent hints preserve existing behavior, malformed hints fall back gracefully).

## Steps

1. Create `src/lsp_hints.rs` with:
   - `LspSymbolHint` struct: `name: String`, `file: String`, `line: u32`, `kind: Option<String>`
   - `LspHints` struct: `symbols: Vec<LspSymbolHint>`
   - `parse_lsp_hints(req: &RawRequest) -> Option<LspHints>` — deserializes from `req.lsp_hints`, logs warning on malformed data, returns `None` on failure
   - `apply_lsp_disambiguation(matches: Vec<SymbolMatch>, hints: &LspHints) -> Vec<SymbolMatch>` — for each match, check if any hint's name+file+line aligns (line within the symbol's range). If exactly one match aligns, return just that match. If no hints match any candidate, return all matches unchanged (fallback). Handle `file://` URI stripping in hint file paths.
   - Unit tests: valid parsing, missing field fallback, malformed JSON → None, disambiguation with single match, disambiguation with no match (fallback), disambiguation with stale hint
2. Register module: add `pub mod lsp_hints;` to `src/lib.rs`
3. Wire into `edit_symbol.rs`: after the scope filter (line ~101-116), before the `filtered.len() > 1` check, call `parse_lsp_hints(req)` and if `Some(hints)`, apply `apply_lsp_disambiguation` to narrow the filtered results
4. Wire into `zoom.rs`: same pattern — after `resolve_symbol`, apply LSP disambiguation before returning ambiguous candidates
5. Wire into `move_symbol.rs`: same pattern at the disambiguation point
6. Wire into `inline_symbol.rs`: same pattern at the disambiguation point
7. Add `tests/integration/lsp_hints_test.rs` with integration tests using the existing `ambiguous.ts` fixture (or a new fixture if needed): (a) edit_symbol with matching lsp_hints → single result, (b) edit_symbol without lsp_hints → ambiguous_symbol candidates, (c) edit_symbol with malformed lsp_hints → falls back to candidates, (d) zoom with matching lsp_hints → single result. Register module in `tests/integration/main.rs`.

## Must-Haves

- [ ] `LspHints` and `LspSymbolHint` structs defined with serde Deserialize
- [ ] `parse_lsp_hints` handles: valid JSON → Some, missing → None, malformed → None with stderr warning
- [ ] `apply_lsp_disambiguation` reduces multiple matches to one when a hint's file+line matches a candidate's range
- [ ] `apply_lsp_disambiguation` returns all matches unchanged when no hint matches (graceful fallback)
- [ ] `file://` URI prefix stripped from hint file paths before comparison
- [ ] All 4 handlers (edit_symbol, zoom, move_symbol, inline_symbol) consume lsp_hints
- [ ] Integration test: ambiguous symbol resolved with lsp_hints
- [ ] Integration test: same symbol without lsp_hints returns candidates (no regression)
- [ ] Integration test: malformed lsp_hints → fallback to candidates
- [ ] `cargo test` passes with zero regressions

## Verification

- `cargo test lsp_hints` — all unit tests for parsing and disambiguation pass
- `cargo test lsp_hints_test` — all integration tests pass
- `cargo test` — full suite passes (baseline 446 + new tests, zero failures)

## Observability Impact

- Signals added: stderr `[aft] lsp_hints: parsed N symbol hints` when hints present and valid; `[aft] lsp_hints: ignoring malformed data: {error}` on parse failure
- How a future agent inspects this: look for `[aft] lsp_hints:` lines in stderr output
- Failure state exposed: malformed hint JSON details logged to stderr; command response unchanged (no error field for hint failures — silent fallback)

## Inputs

- `src/protocol.rs` — `RawRequest.lsp_hints: Option<serde_json::Value>` (entry point, already wired)
- `src/symbols.rs` — `SymbolMatch { symbol: Symbol, file: String }` (the match type to filter)
- `src/commands/edit_symbol.rs:93-149` — existing disambiguation pattern to enhance
- `src/commands/zoom.rs:79-105` — same pattern
- `src/commands/move_symbol.rs:116-135` — same pattern
- `src/commands/inline_symbol.rs:140-160` — same pattern
- `tests/fixtures/` — existing ambiguous.ts fixture with multiple symbols of the same name
- S01/S02 summaries — handler patterns and file locations

## Expected Output

- `src/lsp_hints.rs` — new module with structs, parsing, disambiguation, ~8 unit tests
- `src/lib.rs` — modified with `pub mod lsp_hints`
- `src/commands/edit_symbol.rs` — ~5 lines added for lsp_hints consumption
- `src/commands/zoom.rs` — ~5 lines added
- `src/commands/move_symbol.rs` — ~5 lines added
- `src/commands/inline_symbol.rs` — ~5 lines added
- `tests/integration/lsp_hints_test.rs` — 4+ integration tests
- `tests/integration/main.rs` — `mod lsp_hints_test` added
