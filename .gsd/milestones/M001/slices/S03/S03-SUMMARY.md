---
id: S03
parent: M001
milestone: M001
provides:
  - outline command — nested symbol tree for any supported file
  - zoom command — symbol body with context lines and file-scoped caller/callee annotations
  - src/commands/ module pattern for all future command handlers
  - provider dispatch wiring in main.rs (TreeSitterProvider active)
requires:
  - slice: S02
    provides: TreeSitterProvider (list_symbols, resolve_symbol), FileParser, Symbol types, per-language query files
  - slice: S01
    provides: protocol types (RawRequest, Response), AftError variants, persistent process loop, dispatch function
affects:
  - S05
key_files:
  - src/commands/mod.rs
  - src/commands/outline.rs
  - src/commands/zoom.rs
  - src/main.rs
  - src/lib.rs
  - tests/fixtures/calls.ts
  - tests/integration/commands_test.rs
key_decisions:
  - D021: Command module pattern — per-command files under src/commands/, each exporting handle_*(req, provider) -> Response
  - D022: Zoom caller/callee scope is file-scoped only (cross-file deferred to M003)
  - D023: Zoom handler creates own FileParser rather than extending LanguageProvider trait — avoids RefCell borrow conflicts
  - D024: Call extraction uses recursive AST walk with byte-range containment, not query patterns — single code path for all languages
patterns_established:
  - Command handler signature: handle_X(req: &RawRequest, provider: &dyn LanguageProvider) -> Response
  - Commands module structure: src/commands/mod.rs re-exports, per-command .rs files
  - Call extraction: extract_calls_in_range(source, root_node, byte_start, byte_end, lang) -> Vec<(name, line)>
  - Integration test pattern for commands reuses AftProcess from protocol_test
observability_surfaces:
  - Error responses with structured code + message: symbol_not_found, ambiguous_symbol, file_not_found, invalid_request
  - Ambiguous symbol errors include candidates array with scope-qualified names
  - stderr [aft] prefix logs for parse failures and unknown commands
drill_down_paths:
  - .gsd/milestones/M001/slices/S03/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S03/tasks/T02-SUMMARY.md
duration: 2 tasks
verification_result: passed
completed_at: 2026-03-14
---

# S03: Structural Reading

**Outline and zoom commands delivering nested symbol structure and symbol-body extraction with file-scoped caller/callee annotations, wired through the binary protocol.**

## What Happened

Built the `src/commands/` module pattern and delivered both structural reading commands.

**Outline (T01):** Handler calls `list_symbols()` on the provider and builds a nested `OutlineEntry` tree. Symbols with a parent are placed under their parent using scope-chain matching for multi-level nesting (e.g. `OuterClass → InnerClass → method`). Orphan children whose parent isn't found get promoted to top level defensively. Wired `TreeSitterProvider` into dispatch — renamed `_provider` to `provider`, threaded `&dyn LanguageProvider` through the dispatch function.

**Zoom (T02):** Handler resolves a symbol via `resolve_symbol()`, extracts the body from source lines using 0-based ranges, adds configurable context_before/context_after (default 3, clamped to file boundaries). AST-based call annotation walks the tree within byte ranges to find call expressions — TS/JS/Go use `call_expression`, Python uses `call`, Rust uses `call_expression` + `macro_invocation`. Member access calls (`this.method()`, `obj.fn()`) use a last-segment heuristic extracting from property_identifier/field_identifier children. Callee names are matched against known file-scoped symbols to build `calls_out` and `called_by` arrays. Ambiguous symbol names (multiple matches from `resolve_symbol`) return an `ambiguous_symbol` error with candidates for disambiguation.

Created `tests/fixtures/calls.ts` with a multi-function call graph: helper→compute→orchestrate chain, Calculator class with member calls, unused function, and arrow function — providing concrete edges for call annotation verification.

## Verification

- `cargo test --lib -- commands::outline::tests` — 7/7 passed ✅
- `cargo test --lib -- commands::zoom::tests` — 12/12 passed ✅
- `cargo test --test integration` — 12/12 passed ✅
- `cargo build` — 0 errors, 0 warnings ✅
- Full suite: 84 tests (72 unit + 12 integration), 0 failures, 0 regressions

## Requirements Advanced

- R003 (Structural reading) — outline returns nested symbol tree, zoom returns body with context and annotations. Both wired through binary protocol.
- R011 (Symbol disambiguation) — zoom returns `ambiguous_symbol` error with candidates when multiple symbols match. S03 support complete; full validation deferred to S05 (edit_symbol).

## Requirements Validated

- R003 — outline and zoom commands verified by 19 unit tests + 8 integration tests covering nested structures, all symbol kinds, call annotations, context lines, error paths, and multi-language fixtures (TS, Python, Rust).

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

None.

## Known Limitations

- Caller/callee annotations are file-scoped only — cross-file call graph is M003 (R020)
- Zoom creates a separate FileParser instance for AST walking rather than sharing the provider's cached tree — one extra parse per zoom call on cache miss
- Call extraction relies on text-matching callee names against known symbols — indirect calls (function pointers, dynamic dispatch) are not detected

## Follow-ups

- none — S05 will consume outline/zoom as dependencies for edit_symbol

## Files Created/Modified

- `src/commands/mod.rs` — module declarations for outline and zoom
- `src/commands/outline.rs` — OutlineEntry type, handle_outline handler, flat-to-tree nesting, 7 unit tests
- `src/commands/zoom.rs` — full zoom handler with call extraction, context lines, disambiguation, 12 unit tests
- `src/main.rs` — renamed _provider → provider, wired through dispatch, added outline + zoom arms
- `src/lib.rs` — added `pub mod commands`
- `tests/fixtures/calls.ts` — TS fixture with intra-file function calls for zoom testing
- `tests/integration/commands_test.rs` — 8 integration tests (4 outline + 4 zoom)
- `tests/integration/main.rs` — added commands_test module

## Forward Intelligence

### What the next slice should know
- Command handlers follow a strict pattern: `handle_X(req: &RawRequest, provider: &dyn LanguageProvider) -> Response`. S04/S05 should add new handlers in `src/commands/` following this convention.
- `provider` is now actively wired in `dispatch()` in main.rs — new commands just need a match arm and a handler call.
- The `OutlineEntry` and zoom response types are defined in their respective command modules. S05 edit_symbol can reuse `resolve_symbol()` directly from the provider.

### What's fragile
- Zoom's call extraction uses text-matching of callee names against known symbols — if a local variable shadows a function name, it would produce a false positive. Acceptable for file-scoped analysis but would need refinement for cross-file (M003).
- Member access call heuristic extracts the last segment only — `a.b.c()` reports `c` as the callee. This is correct for method calls but loses the chain context.

### Authoritative diagnostics
- `cargo test --test integration -- test_zoom` and `test_outline` — these exercise the full binary protocol path and are the most trustworthy signal for whether structural reading works end-to-end.
- Error response codes (`symbol_not_found`, `ambiguous_symbol`, `file_not_found`, `invalid_request`) are tested in both unit and integration tests.

### What assumptions changed
- No assumptions changed — both commands implemented as planned with no deviations.
