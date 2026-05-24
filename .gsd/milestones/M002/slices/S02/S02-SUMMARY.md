---
id: S02
parent: M002
milestone: M002
provides:
  - src/indent.rs shared indentation detection utility (detect tabs vs spaces, width, language-specific defaults)
  - add_member command for scope-aware insertion into classes/structs/impl blocks with 4 position modes
  - add_derive command for Rust derive macro append/create on structs and enums
  - wrap_try_catch command for TS/JS function body wrapping with re-indentation
  - add_decorator command for Python decorator insertion with position control
  - add_struct_tags command for Go struct field tag add/update
  - Plugin tool registrations for all 5 commands with Zod schemas
  - ScopeNotFound, MemberNotFound error variants; target_not_found, field_not_found error codes
requires: []
affects:
  - S03
key_files:
  - src/indent.rs
  - src/commands/add_member.rs
  - src/commands/add_derive.rs
  - src/commands/wrap_try_catch.rs
  - src/commands/add_decorator.rs
  - src/commands/add_struct_tags.rs
  - src/error.rs
  - opencode-plugin-aft/src/tools/structure.ts
  - tests/integration/member_test.rs
  - tests/integration/structure_test.rs
key_decisions:
  - "D057: parser.rs node_text and node_range made pub(crate) for scope container detection"
  - "D058: wrap_try_catch limited to statement_block bodies — expression-body arrows deferred"
  - "D059: add_member position:after/before errors on missing member instead of silent fallback"
  - "D060: Rust scope resolution prefers impl blocks over struct items when both share a name"
  - "D061: Compound operations use target_not_found/field_not_found error codes with available lists"
  - "D062: add_derive attribute collection resets on non-attribute siblings"
patterns_established:
  - "Scope container finding per language: walk root children for language-specific node kinds, extract body as (start_byte, end_byte, named_children)"
  - "Compound operation handler pattern: params → validate language → parse AST → find target with available list → transform → backup → write → validate → respond"
  - "Plugin structure tool registration follows D034 pattern: structureTools(bridge) returning Record<string, ToolDefinition>"
observability_surfaces:
  - "stderr log: [aft] add_member/add_derive/wrap_try_catch/add_decorator/add_struct_tags: {file} on every call"
  - "Structured error responses: scope_not_found (with available scopes), member_not_found, target_not_found (with available targets), field_not_found (with available fields)"
  - "Success responses include syntax_valid, backup_id; add_derive returns derives list; add_struct_tags returns tag_string"
drill_down_paths:
  - .gsd/milestones/M002/slices/S02/tasks/T01-SUMMARY.md
  - .gsd/milestones/M002/slices/S02/tasks/T02-SUMMARY.md
  - .gsd/milestones/M002/slices/S02/tasks/T03-SUMMARY.md
duration: 3 tasks
verification_result: passed
completed_at: 2026-03-14
---

# S02: Scope-aware Insertion & Compound Operations

**Scope-aware member insertion for 4 language families and 4 language-specific compound operations (add_derive, wrap_try_catch, add_decorator, add_struct_tags), all registered as OpenCode plugin tools — 35 new integration tests, 249 total tests passing.**

## What Happened

**T01** built the shared indentation detection utility (`src/indent.rs`) and the `add_member` command handler. `detect_indent()` analyzes source lines to determine tabs vs spaces and width, with language-specific defaults (4sp Python/Rust, 2sp TS/JS, tabs Go). The `add_member` handler parses AST, finds scope containers by name per language (TS/JS class_declaration, Python class_definition, Rust impl_item/struct_item, Go type_declaration→struct_type), detects body indentation from existing children, resolves position (first/last/before:name/after:name), indents provided code, and writes with backup and syntax validation. Key decision: Rust impl blocks are preferred over struct items when both share the same name — methods are the more common insertion target.

**T02** built four compound operation handlers. `add_derive` walks backward from Rust struct/enum to find preceding `attribute_item` siblings, merges new derives with dedup, or creates new `#[derive(...)]`. `wrap_try_catch` finds TS/JS functions by name (including class methods and arrow functions in lexical declarations), re-indents body +1 level, wraps in try/catch. `add_decorator` finds Python functions with recursive descent into class bodies and decorated_definition children, inserts `@decorator` with correct indentation and supports first/last positioning for already-decorated functions. `add_struct_tags` parses existing backtick-delimited tag strings into key-value pairs, adds/updates target key, regenerates tag.

**T03** created `structure.ts` with 5 tool definitions following the D034 pattern, wired into `index.ts`, and wrote 14 bun tests covering registration, schema shape, round-trip execution, and error responses.

## Verification

- `cargo build 2>&1 | grep -c warning` → 0
- `cargo test` → 249 total (154 unit + 95 integration), 0 failures
- `cargo test -- member` → 14 passed (TS class last/first/after/empty, Python indentation, Rust struct/impl, Go struct/empty, 4 error paths)
- `cargo test -- structure` → 21 passed (add_derive: 5, wrap_try_catch: 4, add_decorator: 5, add_struct_tags: 7)
- `bun test` → 36 passed across 4 files (14 new structure tests + 22 existing), 0 failures
- Error responses verified with structured `code` field across all commands

## Requirements Advanced

- R014 (Scope-aware member insertion) — `add_member` works for TS/JS classes, Python classes, Rust impl blocks/structs, Go structs with 4 position modes and correct indentation
- R015 (Language-specific compound operations) — all 4 compound operations work through binary protocol with integration tests

## Requirements Validated

- R014 — 14 integration tests prove add_member across 4 language families with positioning, empty containers, and error paths
- R015 — 21 integration tests prove add_derive (Rust), wrap_try_catch (TS/JS), add_decorator (Python), add_struct_tags (Go) through binary protocol with error handling

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- Rust scope resolution order changed from unspecified to explicit impl-first — struct-first caused incorrect behavior when both exist with the same name (T01)
- add_decorator walker needed recursive descent into `class_definition` and `decorated_definition` children — initial flat walk missed nested methods (T02)
- add_derive dedup test changed from global count to pattern check — fixture has Debug on multiple types (T02)

## Known Limitations

- `wrap_try_catch` only handles functions with `statement_block` bodies — arrow functions with expression bodies (`const f = x => x + 1`) are rejected (D058)
- `add_member` position `before:/after:` errors on missing member — no fallback mode (D059)
- No auto-format integration yet — that's S03's responsibility

## Follow-ups

- none — all planned work completed

## Files Created/Modified

- `src/indent.rs` — shared indentation detection utility (160 lines)
- `src/commands/add_member.rs` — scope-aware member insertion handler (450 lines)
- `src/commands/add_derive.rs` — Rust derive manipulation handler (289 lines)
- `src/commands/wrap_try_catch.rs` — TS/JS try-catch wrapping handler (293 lines)
- `src/commands/add_decorator.rs` — Python decorator insertion handler (305 lines)
- `src/commands/add_struct_tags.rs` — Go struct tag manipulation handler (350 lines)
- `src/commands/mod.rs` — added 5 module declarations
- `src/main.rs` — added 5 dispatch arms
- `src/lib.rs` — added `pub mod indent`
- `src/parser.rs` — `node_text` and `node_range` changed to `pub(crate)`
- `src/error.rs` — added ScopeNotFound and MemberNotFound error variants
- `tests/fixtures/member_ts.ts` — TS class fixture
- `tests/fixtures/member_py.py` — Python class fixture (4-space indent)
- `tests/fixtures/member_rs.rs` — Rust struct + impl fixture
- `tests/fixtures/member_go.go` — Go struct fixture
- `tests/fixtures/structure_rs.rs` — Rust fixture with structs/enums and derives
- `tests/fixtures/structure_ts.ts` — TS fixture with functions/class/arrow function
- `tests/fixtures/structure_py.py` — Python fixture with plain/decorated functions and class
- `tests/fixtures/structure_go.go` — Go fixture with struct fields with/without tags
- `tests/integration/member_test.rs` — 14 integration tests
- `tests/integration/structure_test.rs` — 21 integration tests
- `tests/integration/main.rs` — registered member_test and structure_test modules
- `opencode-plugin-aft/src/tools/structure.ts` — 5 tool definitions with Zod schemas
- `opencode-plugin-aft/src/index.ts` — wired structureTools, updated tool category JSDoc
- `opencode-plugin-aft/src/__tests__/structure.test.ts` — 14 tests

## Forward Intelligence

### What the next slice should know
- All 5 S02 commands follow the same handler pattern as S01's import commands — `handle_*(req, ctx) -> Response` with auto-backup, syntax validation, and structured error codes
- The edit pipeline (backup → write → validate) is consistent across all mutation commands — S03 can hook auto-format into `src/edit.rs` and all commands benefit
- Total command count is now 19 (11 M001 + 3 S01 imports + 5 S02 structure) — all need dry_run support in S04

### What's fragile
- Rust scope resolution relies on impl-before-struct ordering — if tree-sitter changes child ordering for Rust, this assumption breaks
- Python decorator detection uses recursive descent that resets on non-decorated_definition nodes — deeply nested decorated functions in unusual patterns could be missed
- Go struct tag parsing handles escaped quotes but assumes well-formed backtick-delimited strings — malformed tags could produce unexpected results

### Authoritative diagnostics
- `cargo test -- member` — 14 tests cover all add_member paths including error codes
- `cargo test -- structure` — 21 tests cover all 4 compound operations including error paths
- `bun test` in opencode-plugin-aft — 14 structure tests including round-trip execution through binary

### What assumptions changed
- Rust scope resolution was unspecified → explicit impl-first with struct fallback (D060)
- add_decorator initially assumed flat function walk → needed recursive descent into class bodies and decorated_definition children
