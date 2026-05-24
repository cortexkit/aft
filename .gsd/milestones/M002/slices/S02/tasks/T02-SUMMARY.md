---
id: T02
parent: S02
milestone: M002
provides:
  - add_derive command for Rust derive macro manipulation
  - wrap_try_catch command for TS/JS function body wrapping
  - add_decorator command for Python decorator insertion
  - add_struct_tags command for Go struct field tag manipulation
key_files:
  - src/commands/add_derive.rs
  - src/commands/wrap_try_catch.rs
  - src/commands/add_decorator.rs
  - src/commands/add_struct_tags.rs
  - tests/integration/structure_test.rs
key_decisions:
  - "add_derive: attribute_item siblings are collected in order and cleared on non-attribute nodes, so only immediately-preceding attributes are checked for derive"
  - "wrap_try_catch: recursive walker finds methods inside class bodies and arrow functions inside lexical_declaration — but only wraps functions with statement_block bodies (not expression-body arrows)"
  - "add_decorator: recursive walker enters class_definition and decorated_definition children to find nested methods (e.g. @staticmethod helper inside a class)"
  - "add_struct_tags: tag parsing handles space-separated key:\"value\" pairs with escaped quote support; existing tag is replaced in-place, new tag appended after type end byte"
patterns_established:
  - "Compound operation handler pattern: params → validate language → parse AST → find target with available list → transform → backup → write → validate → respond with structured result"
  - "Error responses: target_not_found (with available targets), field_not_found (with available fields) — consistent with scope_not_found from add_member"
observability_surfaces:
  - "[aft] add_derive: {file} / [aft] wrap_try_catch: {file} / [aft] add_decorator: {file} / [aft] add_struct_tags: {file} on stderr per invocation"
  - "Success responses include syntax_valid and backup_id; add_derive returns final derives list; add_struct_tags returns final tag_string"
  - "Error responses carry structured code field: target_not_found, field_not_found, invalid_request"
duration: 1 task
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Compound operations — `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`

**Built four language-specific compound operations covering Rust derive manipulation, TS/JS try-catch wrapping, Python decorator insertion, and Go struct tag management — all with auto-backup, syntax validation, and 21 integration tests.**

## What Happened

Built all four compound operation commands following the established handler pattern from add_member and add_import:

1. **add_derive** (Rust): Finds struct/enum by name, walks backward through preceding sibling `attribute_item` nodes to find existing `#[derive(...)]`. Parses derive names from token tree text, merges new derives with dedup, regenerates attribute. Creates new attribute line when no existing derive found.

2. **wrap_try_catch** (TS/JS): Recursive walker finds functions/methods by name including inside class bodies and arrow functions in lexical_declarations. Extracts statement_block body, re-indents all lines +1 level, wraps in try/catch. Returns error for arrow functions without statement_block bodies. Custom catch_body param supported.

3. **add_decorator** (Python): Recursive walker enters class bodies and decorated_definition children to find nested functions (e.g., `@staticmethod` methods inside a class). For plain functions, inserts `@decorator` line before the def. For already-decorated functions, supports `first` (before all decorators) and `last` (after all decorators, before def) positions. Preserves indentation of the target definition.

4. **add_struct_tags** (Go): Finds struct by name via type_declaration→type_spec→struct_type, then field by field_identifier. Parses existing backtick-delimited tag string into key-value pairs, adds/updates the target key, regenerates tag string. For fields without tags, appends tag after type end byte.

All wired into dispatch via mod.rs and main.rs.

## Verification

- `cargo build 2>&1 | grep -c warning` → 0
- `cargo test -- structure` → 21 passed (add_derive: 5, wrap_try_catch: 4, add_decorator: 5, add_struct_tags: 7)
- `cargo test -- add_derive` → 8 passed (3 unit + 5 integration)
- `cargo test` → 249 total tests, 0 failures, no regressions

Slice-level verification status:
- ✅ `cargo build 2>&1 | grep -c warning` → 0
- ✅ `cargo test` — all existing + new tests pass
- ✅ `cargo test -- member` — add_member tests pass
- ✅ `cargo test -- structure` — compound operation tests pass
- ⬜ `bun test` in `opencode-plugin-aft/` — plugin tool registration (T03)
- ⬜ Error response shape verification via integration tests — covered for all 4 commands

## Diagnostics

- Each command logs `[aft] {command}: {file}` on stderr per invocation
- Success responses include `syntax_valid` (bool) and `backup_id` (string)
- `add_derive` responses include `derives` array with the final merged list
- `add_struct_tags` responses include `tag_string` with the final tag literal
- Error codes: `target_not_found` (with available list), `field_not_found` (with available fields), `invalid_request` (param validation)

## Deviations

- add_decorator walker needed recursive descent into `class_definition` and `decorated_definition` children — the initial flat walk missed nested methods like `@staticmethod def helper()` inside a class. Fixed during test debugging.
- add_derive dedup test initially checked global `Debug` count in file — the fixture has Debug on multiple types. Changed to check that no `#[derive(Debug, Debug` pattern exists.

## Known Issues

None.

## Files Created/Modified

- `src/commands/add_derive.rs` — Rust derive manipulation handler (289 lines)
- `src/commands/wrap_try_catch.rs` — TS/JS try-catch wrapping handler (293 lines)
- `src/commands/add_decorator.rs` — Python decorator insertion handler (305 lines)
- `src/commands/add_struct_tags.rs` — Go struct tag manipulation handler (350 lines)
- `src/commands/mod.rs` — added 3 new module declarations
- `src/main.rs` — added 4 new dispatch arms
- `tests/integration/structure_test.rs` — 21 integration tests
- `tests/integration/main.rs` — registered structure_test module
- `tests/fixtures/structure_rs.rs` — Rust fixture with structs/enums and derives
- `tests/fixtures/structure_ts.ts` — TS fixture with functions/class/arrow function
- `tests/fixtures/structure_py.py` — Python fixture with plain/decorated functions and class
- `tests/fixtures/structure_go.go` — Go fixture with struct fields with/without tags
- `.gsd/milestones/M002/slices/S02/tasks/T02-PLAN.md` — added Observability Impact section
