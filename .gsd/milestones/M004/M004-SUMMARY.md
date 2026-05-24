---
id: M004
provides:
  - move_symbol command — moves top-level symbols between files with full import rewiring across workspace
  - extract_function command — extracts line range into new function with AST-based free variable detection, return value inference, call site replacement
  - inline_symbol command — replaces function call with body, argument substitution, scope conflict detection
  - LspHints parsing and disambiguation in binary for edit_symbol, zoom, move_symbol, inline_symbol
  - Plugin LSP query infrastructure (queryLspHints via OpenCode SDK find.symbols/lsp.status)
  - ToolContext pattern for plugin tool factories bundling bridge + client
  - Shared extract.rs module for refactoring utilities (free variables, scope conflicts, parameter substitution)
  - compute_relative_import_path utility for TS/JS/TSX import path rewriting
  - Plugin refactoring.ts tool group with 3 tools (aft_move_symbol, aft_extract_function, aft_inline_symbol)
key_decisions:
  - D100: move_symbol restricted to top-level symbols
  - D101: extract_function limited to TS/JS/TSX and Python (Rust/Go deferred)
  - D102: inline_symbol restricted to single-return functions
  - D103: scope conflicts reported with suggestions, not auto-resolved
  - D104: LSP integration scoped to workspace symbol verification only
  - D105: move_symbol auto-checkpoints before execution
  - D106: relative path strips TS/JS/TSX extensions for idiomatic imports
  - D109: reverse-order import editing to maintain valid byte offsets
  - D110: import rewriting scoped to TS/JS/TSX
  - D111: canonicalize source/dest paths to match callgraph internals
  - D112: shared extract.rs module for refactoring utilities
  - D113: property access identifiers filtered by parent node field name
  - D114: validate_single_return skips nested function bodies
  - D115: substitute_params uses tree-sitter identifier matching
  - D116: LSP hints consumed at handler level, not in LanguageProvider trait
  - D117: ToolContext bundles bridge + client for all plugin factories
  - D118: LSP disambiguation matches on name + file path suffix + line range
patterns_established:
  - move_symbol multi-file mutation pattern (checkpoint → mutate N files → rollback on failure)
  - extract.rs shared module for refactoring analysis utilities
  - Binary lsp_hints pattern (parse → disambiguate → fallback) inserted between scope filter and ambiguity response
  - Plugin LSP hint injection (queryLspHints → add to params before bridge.send)
  - ToolContext as single extensible context for all plugin tool factories
  - refactoring.ts tool group for related refactoring operations
observability_surfaces:
  - stderr "[aft] move_symbol: {symbol} from {source} to {dest} ({N} consumers updated)"
  - stderr "[aft] extract_function: {name} from {file}:{start}-{end} ({N} params)"
  - stderr "[aft] inline_symbol: {symbol} at {file}:{line}"
  - stderr "[aft] lsp_hints: parsed N symbol hints" / "ignoring malformed data: {error}"
  - move_symbol response: files_modified, consumers_updated, checkpoint_name
  - extract_function response: parameters array, return_type, syntax_valid, backup_id
  - inline_symbol response: call_context, substitutions, conflicts
  - Machine-parseable error codes: unsupported_language, this_reference_in_range, multiple_returns, scope_conflict, call_not_found, not_configured, symbol_not_found
requirement_outcomes:
  - id: R028
    from_status: active
    to_status: validated
    proof: S01 — 28 Rust tests (19 unit + 9 integration) prove move_symbol across 5+ consumer files including aliased imports, dry-run, checkpoint/rollback, error paths. Plugin round-trip verified (40 bun tests).
  - id: R029
    from_status: active
    to_status: validated
    proof: S02 — 21 unit tests + 6 integration tests prove extract_function with free variable detection, return value inference, function generation for TS/JS/TSX and Python. Plugin round-trip verified (42 bun tests).
  - id: R030
    from_status: active
    to_status: validated
    proof: S02 — 17 unit tests + 6 integration tests prove inline_symbol with parameter substitution, single-return validation, scope conflict detection for TS/JS/TSX and Python. Plugin round-trip verified (42 bun tests).
  - id: R031
    from_status: active
    to_status: validated
    proof: M001/S01 established LanguageProvider trait + lsp_hints field. M004/S03 completed it — LspHints parsed in 4 handlers, plugin populates hints for 5 commands. 13 unit + 4 integration tests prove disambiguation and fallback. 13 plugin mock client tests prove connected/disconnected/error paths.
  - id: R033
    from_status: active
    to_status: validated
    proof: S03 — plugin queries lsp.status() → find.symbols() → maps SymbolKind → populates lsp_hints for edit_symbol, zoom, move_symbol, inline_symbol, extract_function. Binary parses and applies disambiguation. 13 plugin mock tests + 4 binary protocol tests + 55 total bun tests.
duration: ~3h45m across 3 slices (S01 ~70min, S02 ~1h55m, S03 ~40min)
verification_result: passed
completed_at: 2026-03-14
---

# M004: Refactoring Primitives

**Three single-call refactoring commands (move_symbol, extract_function, inline_symbol) with full workspace import rewiring, AST-based free variable detection, scope conflict reporting, and LSP-enhanced symbol disambiguation — verified by 463 Rust tests + 55 plugin tests.**

## What Happened

M004 delivered the three highest-value refactoring primitives that replace the most error-prone multi-step agent workflows, plus LSP integration completing the accuracy story planned since M001.

**S01 (Move Symbol)** built the `move_symbol` command — the most complex single operation in AFT. The handler validates the target is a top-level symbol, creates an auto-checkpoint for workspace-level rollback, extracts the symbol text from the source file, removes it (cleaning whitespace), appends it to the destination with export, then discovers all consumer files via `callers_of` and import scanning. Each consumer's import path is rewritten from the old source to the new destination, preserving aliases. All file writes go through `write_format_validate`. Dry-run computes multi-file diffs without disk writes. Integration tests surfaced two real bugs — `callers_of` returns relative paths that needed resolving against `project_root()`, and macOS `/var` → `/private/var` symlink canonicalization. Both fixed in the handler.

**S02 (Extract & Inline)** built a shared `extract.rs` module providing the core refactoring analysis, then two command handlers consuming it. `extract_function` walks the AST within a byte range, classifying identifiers by scope level — enclosing function parameters become function parameters, module-level bindings are skipped, `this`/`self` triggers an error. Property access identifiers are filtered by parent node field name (not node kind). Return value detection handles explicit returns, post-range variable usage, and void. `inline_symbol` validates single-return (skipping nested function bodies), finds the call expression, builds a parameter→argument substitution map using tree-sitter identifier matching, checks scope conflicts, and replaces the call with the substituted body adjusted for context (assignment, standalone, return).

**S03 (LSP Integration)** completed the accuracy story. Binary side: `LspHints` struct with defensive parsing and `apply_lsp_disambiguation` that filters tree-sitter matches by file path suffix + line range alignment. Wired into all 4 disambiguation paths (edit_symbol, zoom, move_symbol, inline_symbol). Plugin side: `ToolContext` type bundling bridge + SDK client for all 8 tool factories, `queryLspHints` function checking LSP server status and querying workspace symbols, LSP hint injection into 5 tool execute functions. Graceful fallback when LSP is unavailable — no server, API error, or empty results all produce unchanged behavior.

The three slices connected cleanly: S01 established the multi-file mutation pattern and refactoring.ts tool group; S02 reused the pattern and added tools to the group; S03 enhanced all commands with LSP disambiguation without changing any command's external behavior when hints are absent.

## Cross-Slice Verification

**Success criterion: Agent moves a function with all imports updated**
S01 integration tests prove move_symbol across 5+ consumer files at different directory depths, aliased import preservation, checkpoint create/restore, and 4 error paths. 28 Rust tests + 1 plugin round-trip. ✅

**Success criterion: Agent extracts a block with 3 free variables into a new function**
S02 unit tests prove free variable classification (enclosing params become parameters, module-level skipped, this/self detected, property access filtered). Integration tests prove end-to-end extraction with correct parameters, return type, and call site replacement for TS and Python. 27 tests total. ✅

**Success criterion: Agent inlines a single-return function call**
S02 unit tests prove parameter substitution (whole-word safe), single-return validation (nested functions skipped), scope conflict detection. Integration tests prove end-to-end inlining with argument substitution and context-aware replacement. 23 tests total. ✅

**Success criterion: LSP hints disambiguate where tree-sitter alone is ambiguous**
S03 integration test `test_lsp_hints_disambiguation` sends a request with `lsp_hints` containing location data that resolves an ambiguous symbol to a single candidate. Absence test confirms graceful fallback. Malformed hints test confirms warning + fallback. 4 binary integration tests + 13 plugin mock tests. ✅

**Success criterion: All 3 new commands support dry_run mode (D071)**
S01: dry-run integration test proves multi-file diffs returned, files unchanged. S02: dry-run integration tests for both extract_function and inline_symbol prove diff + syntax_valid returned, files unchanged. ✅

**Success criterion: All 3 new commands auto-format via write_format_validate (D046, D066)**
All three command handlers route file writes through `write_format_validate`. Confirmed in S01 summary (move_symbol), S02 summary (extract_function and inline_symbol). ✅

**Success criterion: Plugin tools registered with Zod schemas and tested**
S01: `aft_move_symbol` in refactoring.ts with Zod schema, bun round-trip test. S02: `aft_extract_function` and `aft_inline_symbol` added with Zod schemas, 2 bun round-trip tests. S03: all adapted to ToolContext, 55 total bun tests pass. ✅

**Success criterion: cargo test passes (baseline 368 + new)**
S03 final count: 463 total (293 unit + 170 integration), 0 failures. Exceeds baseline by 95 tests. ✅

**Success criterion: bun test passes (baseline 39 + new)**
S03 final count: 55 pass, 0 failures. Exceeds baseline by 16 tests. ✅

## Requirement Changes

- R028 (Move symbol): active → validated — 28 Rust tests + plugin round-trip prove single-call move with multi-file import rewriting, aliased import preservation, checkpoint safety, dry-run preview
- R029 (Extract function): active → validated — 27 tests prove extract_function with free variable detection, return value inference, and function generation for TS/JS/TSX and Python
- R030 (Inline symbol): active → validated — 23 tests prove inline_symbol with parameter substitution, single-return validation, and scope conflict detection for TS/JS/TSX and Python
- R031 (LSP-aware architecture): active → validated — provider interface from M001 + lsp_hints consumption in 4 handlers + plugin population for 5 commands, proven by 17 binary tests + 13 plugin tests
- R033 (LSP integration via plugin mediation): active → validated — plugin→binary LSP data flow proven end-to-end with mock client tests and binary protocol tests

## Forward Intelligence

### What the next milestone should know
- 463 Rust tests + 55 plugin tests is the baseline. Any regression is a real signal.
- The `ToolContext` pattern is established for all plugin tool factories — new tools follow `(ctx: ToolContext)` signature.
- All 29 commands route through the same `dispatch()` function in `main.rs`. Adding command 30 is a one-line match arm + handler module.
- The shared `extract.rs` module provides reusable refactoring analysis (free variables, scope conflicts, substitution) for future operations.
- All 15 mutation commands go through `write_format_validate` — auto-format, syntax validation, and dry-run are automatic.

### What's fragile
- Import path matching (`import_path_matches_file`) uses heuristic extension stripping (`.ts`, `.tsx`, `.js`, `.jsx`) and `index` pattern matching. Path aliases (`@/`), explicit `.mjs` extensions, and unusual import conventions will miss matches.
- Consumer discovery combines `callers_of` + import scan. If the call graph is stale (file watcher hasn't drained), consumers could be missed.
- Body-text scope conflict detection parses function body as a standalone snippet — if tree-sitter can't parse the fragment, some declarations may be missed (conservative: blocks rather than misses).
- LSP kind mapping covers common kinds but unknown SymbolKind values are silently omitted.
- Path suffix matching in `apply_lsp_disambiguation` could false-match if two files share the same filename in different directories.

### Authoritative diagnostics
- `cargo test move_symbol` — 28 tests covering all success and error paths for move_symbol
- `cargo test extract_function` — 27 tests covering extract_function surface
- `cargo test inline_symbol` — 23 tests covering inline_symbol surface
- `cargo test lsp_hints` — 14+ tests covering LSP hint parsing and disambiguation
- `bun test` in `opencode-plugin-aft/` — 55 tests covering all plugin tools and LSP query logic
- Response JSON `consumers_updated` count — quick indicator of move_symbol rewiring completeness
- Machine-parseable error `code` field on all error responses

### What assumptions changed
- Assumed `callers_of` returns absolute paths — actually returns relative paths. Added `project_root()` getter and resolution logic (S01).
- Assumed temp dir paths are canonical on macOS — `/var/folders` vs `/private/var/folders` mismatch required explicit canonicalization (D111, S01).
- SDK LSP surface matched research expectations — `find.symbols()` and `lsp.status()` only, no go-to-definition or find-references (confirmed in S03).

## Files Created/Modified

- `src/commands/move_symbol.rs` — move_symbol handler with multi-file mutation pipeline, 19 unit tests
- `src/commands/extract_function.rs` — extract_function handler, 7 unit tests
- `src/commands/inline_symbol.rs` — inline_symbol handler, 7 unit tests
- `src/extract.rs` — shared refactoring utilities (free variables, scope conflicts, substitution), 24 unit tests
- `src/lsp_hints.rs` — LspHints struct, parse/disambiguate functions, path helpers, 13 unit tests
- `src/commands/mod.rs` — added move_symbol, extract_function, inline_symbol modules
- `src/main.rs` — added 3 dispatch entries
- `src/lib.rs` — added `pub mod extract`, `pub mod lsp_hints`
- `src/callgraph.rs` — added `project_root()` pub getter
- `src/commands/edit_symbol.rs` — lsp_hints disambiguation block
- `src/commands/zoom.rs` — lsp_hints disambiguation block
- `tests/integration/move_symbol_test.rs` — 9 integration tests
- `tests/integration/extract_function_test.rs` — 6 integration tests
- `tests/integration/inline_symbol_test.rs` — 6 integration tests
- `tests/integration/lsp_hints_test.rs` — 4 integration tests
- `tests/integration/main.rs` — added 4 module declarations
- `tests/fixtures/move_symbol/` — 8 fixture files
- `tests/fixtures/extract_function/` — 3 fixture files
- `tests/fixtures/inline_symbol/` — 4 fixture files
- `opencode-plugin-aft/src/tools/refactoring.ts` — 3 refactoring tool definitions with Zod schemas
- `opencode-plugin-aft/src/types.ts` — ToolContext type
- `opencode-plugin-aft/src/lsp.ts` — queryLspHints, LSP_SYMBOL_KIND_MAP
- `opencode-plugin-aft/src/index.ts` — ToolContext construction, updated factory calls
- `opencode-plugin-aft/src/tools/editing.ts` — ToolContext param, lsp_hints for edit_symbol
- `opencode-plugin-aft/src/tools/reading.ts` — ToolContext param, lsp_hints for zoom
- `opencode-plugin-aft/src/tools/navigation.ts` — ToolContext param (signature)
- `opencode-plugin-aft/src/tools/safety.ts` — ToolContext param (signature)
- `opencode-plugin-aft/src/tools/imports.ts` — ToolContext param (signature)
- `opencode-plugin-aft/src/tools/structure.ts` — ToolContext param (signature)
- `opencode-plugin-aft/src/tools/transaction.ts` — ToolContext param (signature)
- `opencode-plugin-aft/src/__tests__/lsp.test.ts` — 13 mock client tests
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — adapted to ToolContext, added round-trip tests
- `opencode-plugin-aft/src/__tests__/structure.test.ts` — adapted to ToolContext
