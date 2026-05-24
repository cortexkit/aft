---
id: T01
parent: S01
milestone: M002
provides:
  - Import analysis engine (src/imports.rs) with TS/JS/TSX parsing, grouping, dedup, and insertion
  - add_import command handler wired into binary dispatch
  - Integration tests proving group placement, dedup, alphabetization, and error paths
key_files:
  - src/imports.rs
  - src/commands/add_import.rs
  - tests/integration/import_test.rs
key_decisions:
  - Import type detection uses direct child node kind="type" on import_statement (tree-sitter-typescript grammar detail)
  - Group classification is simple prefix-based (dot = relative, else external) — no npm registry lookups
  - Dedup compares kind (value vs type) separately — adding a type import won't dedup against a same-module value import
  - grammar_for() made pub in parser.rs for import engine reuse
patterns_established:
  - Import engine architecture: shared types + per-language parse/classify/generate + language dispatch via LangId match
  - Integration test temp files use atomic counter for unique paths to avoid parallel test races
observability_surfaces:
  - stderr log: "[aft] add_import: {file}" on every invocation (including dedup hits)
  - Error responses include code + message fields (invalid_request, file_not_found, parse_error)
  - Response fields: added (bool), already_present (bool), group (string), syntax_valid (bool)
duration: 45min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Import engine core + add_import for TS/JS/TSX

**Built the import analysis engine with TS/JS/TSX tree-sitter parsing and wired add_import into the binary protocol with full integration test coverage.**

## What Happened

Created `src/imports.rs` (~450 lines) with shared types (`ImportStatement`, `ImportBlock`, `ImportGroup`, `ImportKind`) and the TS/JS/TSX implementation. The engine walks tree-sitter AST root children for `import_statement` nodes, extracts module paths, named/default/namespace imports, detects `import type` via direct child node inspection, and classifies into External vs Relative groups.

Key operations: `parse_imports` → `is_duplicate` → `find_insertion_point` → `generate_import_line`. Insertion point logic handles: alphabetical ordering within group, type-after-value sorting, new group creation with blank line separators, and empty-file edge case.

Created `src/commands/add_import.rs` (~160 lines) following the existing handler pattern (`handle_*(req, ctx) -> Response`). Flow: param extraction → file existence check → language detection → language support check → parse → dedup → find insertion → generate → backup → insert → write → validate syntax → respond.

Wired into `src/commands/mod.rs` and `src/main.rs` dispatch.

22 unit tests in `imports.rs` cover parsing (named, default, namespace, side-effect, type, multi-group), classification, dedup (same name, different name, default, side-effect, type-vs-value), generation (all import forms), and insertion point logic.

9 integration tests in `import_test.rs` prove the full protocol round-trip: external group placement, relative group placement, deduplication, alphabetization, JS file support, empty file handling, and three error paths (missing file, unsupported language, missing params).

## Verification

- `cargo test` — 164 tests (120 unit + 44 integration), 0 failures, 0 regressions
- `cargo test -- import` — 31 tests (22 unit + 9 integration), all pass
- `cargo test --test integration` — 44 tests, all pass

### Slice-level verification status (T01 is task 1 of 3):

- ✅ `cargo test` — all existing tests still pass (no regressions)
- ✅ `cargo test -- import` — unit tests for TS/JS/TSX parsing, grouping, dedup, sort pass
- ✅ `cargo test --test integration` — integration tests pass: add_import correct group for TS/JS, dedup, alphabetize
- ⬜ `add_import` for Python/Rust/Go — T02
- ⬜ `remove_import` / `organize_imports` — T03
- ⬜ `bun test` plugin — T03
- ✅ Integration tests verify structured error responses: `add_import` on missing file returns `code: "file_not_found"`, unsupported language returns `code: "invalid_request"`

## Diagnostics

- Send `add_import` with a module+name and check `already_present` / `group` / `syntax_valid` in response to inspect import state
- Error responses include machine-readable `code` field (invalid_request, file_not_found)
- stderr shows `[aft] add_import: {file}` on every call

## Deviations

- Made `grammar_for()` pub in `parser.rs` — the import engine needs to create its own parser instances for file-level parsing outside the cached `FileParser`
- TS fixture originally had JSX syntax (`<div>`) which fails TS-only parsing — fixed to pure TS
- `import type` detection: plan assumed it might be inside `import_clause` — tree-sitter puts it as a direct child of `import_statement`

## Known Issues

- Python/Rust/Go import support returns empty ImportBlock (stub) — to be implemented in T02
- No `import type` handling in JS (JS doesn't have `import type` — correct behavior, not a bug)

## Files Created/Modified

- `src/imports.rs` — new import analysis engine with shared types + TS/JS/TSX implementation (~450 lines)
- `src/commands/add_import.rs` — new command handler (~160 lines)
- `src/commands/mod.rs` — registered add_import module
- `src/main.rs` — added dispatch entry for add_import command
- `src/parser.rs` — made `grammar_for()` pub
- `src/lib.rs` — registered imports module
- `tests/fixtures/imports_ts.ts` — TS fixture with 3 import groups (external, relative, type)
- `tests/fixtures/imports_js.js` — JS fixture with external and relative imports
- `tests/integration/import_test.rs` — 9 integration tests
- `tests/integration/main.rs` — registered import_test module
- `.gsd/milestones/M002/slices/S01/S01-PLAN.md` — added failure-path verification step
- `.gsd/milestones/M002/slices/S01/tasks/T01-PLAN.md` — added Observability Impact section
