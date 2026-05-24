---
id: T01
parent: S03
milestone: M004
provides:
  - LspHints struct and parsing from RawRequest.lsp_hints
  - apply_lsp_disambiguation function filtering SymbolMatch by file+line
  - LSP disambiguation wired into edit_symbol, zoom, move_symbol, inline_symbol
  - 13 unit tests for parsing/disambiguation + 4 integration tests for protocol-level behavior
key_files:
  - src/lsp_hints.rs
  - src/commands/edit_symbol.rs
  - src/commands/zoom.rs
  - src/commands/move_symbol.rs
  - src/commands/inline_symbol.rs
  - tests/integration/lsp_hints_test.rs
key_decisions:
  - Disambiguation uses index-based filtering to preserve original matches on fallback (avoids consuming the vec)
  - Path matching uses suffix comparison to handle absolute vs relative paths from LSP
  - inline_symbol gets LSP disambiguation before the find(function/method) filter, so hints narrow the candidate pool
patterns_established:
  - lsp_hints consumption pattern: parse_lsp_hints(req) → if Some, apply_lsp_disambiguation(matches, &hints). Insert between scope filter and ambiguity check.
observability_surfaces:
  - stderr "[aft] lsp_hints: parsed N symbol hints" when valid hints present
  - stderr "[aft] lsp_hints: ignoring malformed data: {error}" on parse failure
duration: 25m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Binary-side lsp_hints consumption and disambiguation

**Added `LspHints` struct, defensive parsing, and disambiguation logic to the binary. Wired into all 4 command handlers. 293 unit + 170 integration tests pass.**

## What Happened

Created `src/lsp_hints.rs` with:
- `LspSymbolHint` struct (name, file, line, kind) and `LspHints` struct (symbols vec), both serde-deserializable
- `parse_lsp_hints(req)` — extracts and deserializes from `req.lsp_hints`, returns `None` with stderr warning on malformed data
- `apply_lsp_disambiguation(matches, hints)` — uses index-based filtering to check if any hint's name+file+line aligns with a candidate's range. Returns the single aligned match if exactly one hits; returns all original matches unchanged otherwise (graceful fallback)
- `strip_file_uri()` and `paths_match()` helpers for path normalization
- 13 unit tests covering valid/absent/malformed parsing, single-match disambiguation, no-match fallback, stale hints, file:// URI stripping, suffix path matching

Registered module in `src/lib.rs`. Wired `parse_lsp_hints` + `apply_lsp_disambiguation` into all 4 handlers:
- `edit_symbol.rs` — after scope filter, before `filtered.len() > 1` check
- `zoom.rs` — after `resolve_symbol`, before `matches.len() > 1` check
- `move_symbol.rs` — after scope filter, before `filtered.is_empty()` check
- `inline_symbol.rs` — after `resolve_symbol`, before `.find(|m| ...)` function/method filter

Added 4 integration tests in `tests/integration/lsp_hints_test.rs`:
1. edit_symbol with matching lsp_hints → success (single result)
2. edit_symbol without lsp_hints → ambiguous_symbol candidates
3. edit_symbol with malformed lsp_hints → fallback to candidates + stderr warning
4. zoom with matching lsp_hints → success (single result)

## Verification

- `cargo test lsp_hints` — 14 unit tests pass (13 in lsp_hints module + 1 existing protocol test)
- `cargo test lsp_hints_test` — 4 integration tests pass
- `cargo test` — full suite: 293 unit + 170 integration = 463 total, zero failures
- Slice-level checks: `cargo test lsp_hints` ✅, `cargo test edit_symbol` ✅ (passes — integration tests include lsp_hints cases), `cargo test zoom` ✅ (passes with lsp_hints test). `bun test` in plugin — not applicable for T01 (T02 scope).

## Diagnostics

- Observability: grep stderr for `[aft] lsp_hints:` to see parsing outcomes
- `parsed N symbol hints` on valid data, `ignoring malformed data: {detail}` on failures
- No new response fields — disambiguation is silent when successful

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/lsp_hints.rs` — new: LspHints/LspSymbolHint structs, parse_lsp_hints, apply_lsp_disambiguation, path helpers, 13 unit tests
- `src/lib.rs` — modified: added `pub mod lsp_hints`
- `src/commands/edit_symbol.rs` — modified: added lsp_hints import + 5-line disambiguation block after scope filter
- `src/commands/zoom.rs` — modified: added lsp_hints import + 5-line disambiguation block after resolve_symbol
- `src/commands/move_symbol.rs` — modified: added lsp_hints import + 5-line disambiguation block after scope filter
- `src/commands/inline_symbol.rs` — modified: added lsp_hints import + 5-line disambiguation block before function/method filter
- `tests/integration/lsp_hints_test.rs` — new: 4 integration tests for protocol-level lsp_hints behavior
- `tests/integration/main.rs` — modified: added `mod lsp_hints_test`
