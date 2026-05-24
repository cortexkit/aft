---
id: S01
parent: M002
milestone: M002
provides:
  - Import analysis engine (src/imports.rs) with per-language parsing, grouping, dedup, and insertion for all 6 languages
  - add_import command with language-aware group placement, dedup, alphabetization
  - remove_import command with full-statement and partial-name removal
  - organize_imports command with re-grouping, sorting, dedup, and Rust use-tree merging
  - All 3 import commands registered in OpenCode plugin with Zod schemas
  - Unified 3-tier ImportGroup enum (Stdlib/External/Internal) across all languages
requires:
  - slice: M001/S01–S06
    provides: Binary protocol, AppContext dispatch, auto_backup, validate_syntax, FileParser, LangId, tree-sitter grammars, plugin BinaryBridge
affects:
  - S03
key_files:
  - src/imports.rs
  - src/commands/add_import.rs
  - src/commands/remove_import.rs
  - src/commands/organize_imports.rs
  - opencode-plugin-aft/src/tools/imports.ts
  - tests/integration/import_test.rs
key_decisions:
  - "D048: Import engine as single module with LangId dispatch — shared types + per-language logic in one file"
  - "D049: Python stdlib detection via embedded list — no runtime subprocess dependency"
  - "D050: Go import grouping by dot heuristic — matches goimports convention"
  - "D051: grammar_for() made pub for import engine reuse"
  - "D052: Type import dedup is kind-aware — value and type imports don't dedup against each other"
  - "D053: ImportGroup unified 3-tier enum (Stdlib/External/Internal) replacing External/Relative"
  - "D054: Whole-module dedup matches on module path alone for Python/Rust/Go"
  - "D055: organize_imports Rust merge groups by (prefix, kind, is_pub) tuple"
  - "D056: remove_import name-removes entire statement when single name"
patterns_established:
  - "Import engine architecture: shared types + per-language parse/classify/generate + LangId match dispatch"
  - "Import command handlers follow same structure: extract params → validate file/lang → parse_file_imports → auto_backup → mutate → write → validate_syntax → respond"
  - "Plugin tool registration: each tool category gets its own file (reading.ts, editing.ts, safety.ts, imports.ts)"
  - "Integration test temp files use atomic counter for unique paths"
observability_surfaces:
  - "stderr: [aft] add_import/remove_import/organize_imports: {file} on every call"
  - "Error responses include code + message fields (invalid_request, file_not_found, import_not_found, parse_error)"
  - "add_import response: added, already_present, group, syntax_valid"
  - "remove_import response: removed, syntax_valid"
  - "organize_imports response: groups (name + count), removed_duplicates, syntax_valid"
drill_down_paths:
  - .gsd/milestones/M002/slices/S01/tasks/T01-SUMMARY.md
  - .gsd/milestones/M002/slices/S01/tasks/T02-SUMMARY.md
  - .gsd/milestones/M002/slices/S01/tasks/T03-SUMMARY.md
duration: ~3h across 3 tasks
verification_result: passed
completed_at: 2026-03-14
---

# S01: Import Management

**Language-aware import management across 6 languages — add, remove, and organize imports with correct group placement, dedup, alphabetization, and Rust use-tree merging, through both binary protocol and OpenCode plugin.**

## What Happened

Built the import analysis engine in `src/imports.rs` (~750 lines) supporting TypeScript, JavaScript, TSX, Python, Rust, and Go. Each language gets tree-sitter AST walking to detect import nodes, parse them into structured `ImportStatement` records, classify into groups, and generate new import text.

**T01** established the engine architecture with shared types (`ImportStatement`, `ImportBlock`, `ImportGroup`, `ImportKind`) and TS/JS/TSX implementations. Key design: the engine walks root-level AST children to find import nodes, extracts module paths and names, detects `import type` via direct child node inspection, and classifies into External vs Relative groups. Built the `add_import` command handler with the full flow: param extraction → file check → language detection → parse → dedup → find insertion point → generate → backup → insert → validate → respond.

**T02** extended to Python, Rust, and Go, requiring a refactor of `ImportGroup` from 2-tier (External/Relative) to 3-tier (Stdlib/External/Internal). Python uses an embedded stdlib module list for isort-style classification. Rust groups by first path segment (std/core/alloc → Stdlib, crate/self/super → Internal, else → External). Go uses the dot-in-path heuristic matching goimports convention. Extended dedup to handle whole-module imports (Python/Rust/Go don't use side-effect kind for plain imports).

**T03** completed the command surface with `remove_import` (full-statement and partial-name removal) and `organize_imports` (re-group, re-sort, dedup, and Rust use-tree merging per D045). Created plugin tool definitions for all 3 commands with Zod schemas following the D034 pattern.

## Verification

- `cargo build` — 0 warnings ✅
- `cargo test` — 202 tests (141 unit + 61 integration), 0 failures, 0 regressions ✅
- `cargo test -- import` — 69 tests (43 unit + 26 integration), all pass ✅
- `bun test` — 22 plugin tests, 0 failures ✅

Integration tests prove:
- `add_import` places imports in correct group for all 6 languages ✅
- `add_import` deduplicates (already-present returns success without modification) ✅
- `add_import` alphabetizes within group ✅
- `remove_import` removes entire statement ✅
- `remove_import` removes one name from multi-name import ✅
- `organize_imports` re-sorts and re-groups ✅
- `organize_imports` merges Rust use declarations with common prefixes ✅
- Error responses: unsupported language → `invalid_request`, missing file → `file_not_found`, missing module → `import_not_found` ✅

## Requirements Advanced

- R013 — All 3 import commands (add_import, remove_import, organize_imports) work across 6 languages through binary protocol and plugin
- R034 — Web-first ordering followed: TS/JS/TSX implemented first in T01, Python/Rust/Go in T02

## Requirements Validated

- R013 — Import management proven by 26 integration tests covering group placement, dedup, alphabetization, removal, and organization across all 6 languages, plus 43 unit tests for parsing and classification

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- `ImportGroup` refactored from 2-tier to 3-tier (D053) — not planned in T01 but required when extending to Python/Rust/Go in T02. All existing TS/JS tests updated from "relative" to "internal" labels.
- `is_duplicate` extended for whole-module imports (D054) — dedup needed to handle Python/Rust/Go module-level imports that use Value kind rather than SideEffect.
- `grammar_for()` made pub (D051) — import engine needs its own parser instances, not originally planned.

## Known Limitations

- Rust `pub use` stored in `default_import` field as "pub" marker — works but a dedicated `is_pub` flag would be cleaner
- Python `from . import utils` parses module_path as "." — works for classification but raw path may surprise consumers
- Go `add_import` doesn't convert single imports to grouped form — deferred to `organize_imports`
- Top-level imports only (D041) — nested/conditional imports not supported
- Imports are correctly placed but not auto-formatted until S03

## Follow-ups

- none — all planned work for S01 completed

## Files Created/Modified

- `src/imports.rs` — import analysis engine with shared types + 6 language implementations (~750 lines)
- `src/commands/add_import.rs` — add_import command handler (~160 lines)
- `src/commands/remove_import.rs` — remove_import command handler (~210 lines)
- `src/commands/organize_imports.rs` — organize_imports handler with Rust use-tree merging (~460 lines)
- `src/commands/mod.rs` — registered 3 new command modules
- `src/main.rs` — 3 new dispatch entries
- `src/parser.rs` — grammar_for() made pub
- `src/lib.rs` — registered imports module
- `opencode-plugin-aft/src/tools/imports.ts` — plugin tool definitions for 3 import commands (~90 lines)
- `opencode-plugin-aft/src/index.ts` — registered importTools in tool registry
- `tests/fixtures/imports_ts.ts` — TS fixture with 3 import groups
- `tests/fixtures/imports_js.js` — JS fixture with import groups
- `tests/fixtures/imports_py.py` — Python fixture with isort-style groups
- `tests/fixtures/imports_rs.rs` — Rust fixture with use declarations
- `tests/fixtures/imports_go.go` — Go fixture with grouped/single imports
- `tests/integration/import_test.rs` — 26 integration tests for all 3 commands
- `tests/integration/main.rs` — registered import_test module

## Forward Intelligence

### What the next slice should know
- The import engine in `src/imports.rs` is the authoritative module for any import-related analysis. It exports `parse_file_imports()` as the main entry point for any code needing import block data.
- Command handlers follow a strict pattern: extract params → validate file/lang → parse → backup → mutate → write → validate syntax → respond. S03's auto-format hook should insert between "write" and "validate syntax" steps.
- Plugin tool definitions in `opencode-plugin-aft/src/tools/` follow a category-per-file pattern. Each file exports a function taking BinaryBridge and returning `Record<string, ToolDefinition>`.

### What's fragile
- `src/imports.rs` at ~750 lines is approaching the split threshold (D048 sets it at ~800). Adding more per-language logic should trigger the submodule refactor.
- Python stdlib list is static (D049). If a user's project uses a module that was added in a newer Python version, it'll be classified as third-party.

### Authoritative diagnostics
- `cargo test -- import` — 69 tests covering all language parsing, classification, dedup, and command round-trips. First thing to run if any import behavior is questioned.
- Error responses include machine-readable `code` field — grep stderr for `[aft] add_import:` / `remove_import:` / `organize_imports:` to trace command flow.

### What assumptions changed
- Import grouping was initially designed as 2-tier (External/Relative) — actual implementation required 3-tier (Stdlib/External/Internal) to handle Python and Rust properly.
- Tree-sitter `import type` detection assumed the type keyword would be nested in import_clause — it's actually a direct child of `import_statement`.
