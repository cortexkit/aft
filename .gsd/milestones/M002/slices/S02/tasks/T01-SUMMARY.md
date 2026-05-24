---
id: T01
parent: S02
milestone: M002
provides:
  - src/indent.rs shared indentation detection utility
  - add_member command handler for scope-aware member insertion
  - ScopeNotFound and MemberNotFound error variants
key_files:
  - src/indent.rs
  - src/commands/add_member.rs
  - src/error.rs
  - tests/integration/member_test.rs
key_decisions:
  - "Rust scope resolution: impl blocks preferred over struct items when both share the same name — methods are the more common add_member target"
  - "Indent detection uses smallest observed indent width as the unit, not GCD of candidates — produces correct 4-space detection for Rust/Python files"
patterns_established:
  - "Scope container finding per language: walk root children for language-specific node kinds, extract body info as (start_byte, end_byte, named_children)"
  - "BodyChild struct for position resolution: carries name + byte range, enables before:/after: member lookup"
observability_surfaces:
  - "stderr log: [aft] add_member: {file} on every successful call"
  - "structured error responses: scope_not_found (lists available scopes), member_not_found (names member and scope)"
duration: 1h
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Indentation utility and `add_member` command

**Built shared indent detection (`src/indent.rs`) and scope-aware `add_member` command covering TS/JS classes, Python classes, Rust structs/impl blocks, and Go structs with correct indentation and 4 position modes.**

## What Happened

Built `src/indent.rs` with `detect_indent(source, lang) -> IndentStyle` that analyzes leading whitespace on source lines to determine tabs vs spaces and width. Uses smallest-observed-indent-width heuristic with language-specific defaults as fallback (Python 4sp, TS/JS 2sp, Rust 4sp, Go tabs). Confidence gate: >50% of indented lines must agree, else default.

Made `node_text()` and `node_range()` `pub(crate)` in `parser.rs` — both needed by scope container detection.

Added `ScopeNotFound` and `MemberNotFound` error variants to `error.rs` with structured `code` + `message` fields. `ScopeNotFound` includes the scope name searched and list of available scopes in the file.

Built `src/commands/add_member.rs` (~450 lines) following the established handler pattern. Per-language scope container resolution:
- **TS/JS**: `class_declaration` → `class_body`, including export-wrapped classes
- **Python**: `class_definition` → `block`, including decorated classes
- **Rust**: `impl_item` → `declaration_list` (preferred) or `struct_item` → `field_declaration_list`
- **Go**: `type_declaration` → `type_spec` → `struct_type` → `field_declaration_list`

Position resolution supports `first`, `last`, `before:name`, `after:name`. Body children are extracted with names for member lookup. Indentation is detected from existing children or falls back to file-level default.

Key design decision: for Rust, impl blocks are searched before struct items when scope names collide (`struct Config` + `impl Config`). Impl is the more common target for method insertion.

## Verification

- `cargo build 2>&1 | grep -c warning` → 0 ✅
- `cargo test -- detect_indent` → 6 unit tests pass ✅
- `cargo test -- member` → 14 integration tests pass ✅
  - TS: class last, class first, after:name, empty class
  - Python: class last, indentation matches (4-space verified)
  - Rust: struct field (EmptyStruct), impl method (Config)
  - Go: struct field, empty struct
  - Errors: scope_not_found, member_not_found, file_not_found, missing_params
- `cargo test` → 75 total tests pass, 0 failures, no regressions ✅

Slice-level checks (partial — T01 is intermediate):
- ✅ `cargo build` → 0 warnings
- ✅ `cargo test` → all pass
- ✅ `cargo test -- member` → all 14 pass
- 🔲 `cargo test -- structure` — T02 scope
- 🔲 `bun test` — T03 scope
- ✅ Error responses verified with structured code field

## Diagnostics

- `[aft] add_member: {file}` on stderr for every successful call
- Error responses carry structured `code` field: `scope_not_found` includes `available` scope list in message, `member_not_found` includes member name and scope
- Invalid position values return `invalid_request` with supported values listed

## Deviations

- Rust scope resolution order changed from "struct first" to "impl first" — the plan said "walk for struct_item and impl_item" without specifying order, but struct-first caused incorrect behavior when both exist with the same name.
- `add_member_rs_struct_field` test updated to use `EmptyStruct` (no impl block) instead of `Config` (has both struct and impl) to properly test struct field insertion.

## Known Issues

None.

## Files Created/Modified

- `src/indent.rs` — new shared indentation detection utility (160 lines)
- `src/commands/add_member.rs` — new scope-aware member insertion handler (450 lines)
- `src/commands/mod.rs` — added `pub mod add_member`
- `src/main.rs` — added `add_member` dispatch arm
- `src/lib.rs` — added `pub mod indent`
- `src/parser.rs` — changed `node_text` and `node_range` from `fn` to `pub(crate) fn`
- `src/error.rs` — added `ScopeNotFound` and `MemberNotFound` error variants with Display and code()
- `tests/fixtures/member_ts.ts` — TS class fixture (UserService + EmptyClass)
- `tests/fixtures/member_py.py` — Python class fixture (4-space indent)
- `tests/fixtures/member_rs.rs` — Rust struct + impl fixture
- `tests/fixtures/member_go.go` — Go struct fixture
- `tests/integration/member_test.rs` — 14 integration tests
- `tests/integration/main.rs` — registered `member_test` module
- `.gsd/milestones/M002/slices/S02/S02-PLAN.md` — added diagnostic verification step
- `.gsd/milestones/M002/slices/S02/tasks/T01-PLAN.md` — added Observability Impact section
