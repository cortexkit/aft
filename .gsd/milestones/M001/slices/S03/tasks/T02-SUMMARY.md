---
id: T02
parent: S03
milestone: M001
provides:
  - zoom command handler with call extraction, context lines, symbol disambiguation
  - calls.ts fixture for zoom testing with intra-file call graph
key_files:
  - src/commands/zoom.rs
  - tests/fixtures/calls.ts
  - tests/integration/commands_test.rs
key_decisions:
  - Zoom handler creates its own FileParser for AST walking rather than extending LanguageProvider trait — keeps trait minimal and avoids borrow conflicts with RefCell-based TreeSitterProvider
  - Call extraction uses recursive walk_for_calls with byte-range containment check rather than query patterns — simpler, works across all 6 languages with a single code path
  - Member access calls use last-segment heuristic (extract property_identifier/field_identifier from end of member_expression) — correct for this.method() and obj.fn() patterns
  - Rust macros matched as macro_invocation node kind with "!" appended to name — keeps them distinguishable from function calls
patterns_established:
  - Call extraction pattern: extract_calls_in_range(source, root_node, byte_start, byte_end, lang) → Vec<(name, line)>
  - line_col_to_byte helper for converting 0-based Range to byte offsets for AST walking
observability_surfaces:
  - zoom error responses use symbol_not_found, ambiguous_symbol, file_not_found, invalid_request codes
  - ambiguous_symbol error includes candidates array with scope-qualified names for disambiguation
duration: 25m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Zoom command with caller/callee annotations

**Implemented zoom command with symbol body extraction, context lines, and file-scoped caller/callee call annotations across all 6 supported languages.**

## What Happened

Built `src/commands/zoom.rs` with the full zoom handler: symbol resolution via `resolve_symbol()`, body extraction from 0-based line ranges, configurable context_before/context_after (default 3 lines, clamped to file boundaries), and AST-based call annotation.

Call extraction walks the tree-sitter AST within byte ranges to find call_expression (TS/JS/Go), call (Python), and call_expression + macro_invocation (Rust) nodes. For member access calls like `this.add()`, a last-segment heuristic extracts the method name from property_identifier/field_identifier children. Callee names are matched against known file-scoped symbols to build `calls_out` and `called_by` arrays.

Created `tests/fixtures/calls.ts` with a multi-function call graph: helper→compute→orchestrate chain, a Calculator class with member access calls, an unused function, and an arrow function — providing concrete edges for call annotation verification.

Wired zoom into main.rs dispatch. Added 4 integration tests (success with annotations, symbol not found, context_lines parameter, empty annotations arrays) and 12 unit tests covering call extraction, context line clamping, body extraction, and error paths.

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test --lib -- commands::zoom::tests` — 12 tests passed ✅
- `cargo test --test integration -- test_zoom` — 4 tests passed ✅
- `cargo test` — all 84 tests passed (72 unit + 12 integration) ✅

Slice-level verification (all checks pass — this is the final task):
- `cargo test --lib -- commands::outline::tests` — 7 passed ✅
- `cargo test --lib -- commands::zoom::tests` — 12 passed ✅
- `cargo test --test integration` — 12 passed ✅
- `cargo build` — 0 errors, 0 warnings ✅

## Diagnostics

Send `{"id":"1","command":"zoom","file":"path/to/file.ts","symbol":"functionName"}` through binary protocol. Response contains `name`, `kind`, `range`, `content`, `context_before`, `context_after`, `annotations.calls_out`, `annotations.called_by`. Error cases: missing params → `invalid_request`, nonexistent file → `file_not_found`, symbol not found → `symbol_not_found`, multiple matches → `ambiguous_symbol` with candidates. All include structured `code` + `message`.

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/commands/zoom.rs` — full zoom handler with call extraction, context lines, disambiguation, 12 unit tests
- `src/commands/mod.rs` — already exported zoom from T01 stub
- `src/main.rs` — added zoom dispatch arm
- `tests/fixtures/calls.ts` — TS fixture with intra-file function calls for zoom testing
- `tests/integration/commands_test.rs` — added 4 zoom integration tests
