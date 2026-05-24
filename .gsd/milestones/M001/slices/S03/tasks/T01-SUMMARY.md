---
id: T01
parent: S03
milestone: M001
provides:
  - outline command handler with flat-to-tree nesting
  - commands module pattern for future commands
  - provider dispatch wiring in main.rs
key_files:
  - src/commands/mod.rs
  - src/commands/outline.rs
  - src/main.rs
  - src/lib.rs
  - tests/integration/commands_test.rs
key_decisions:
  - Scope-chain walk for multi-level nesting rather than name-only parent lookup — handles Python OuterClass.InnerClass.inner_method correctly
  - Orphan children (parent not found in symbol list) promoted to top level defensively
patterns_established:
  - Command handler pattern: `handle_X(req: &RawRequest, provider: &dyn LanguageProvider) -> Response`
  - Commands module structure: src/commands/mod.rs + per-command files
  - Integration test pattern for commands reuses AftProcess from protocol_test
observability_surfaces:
  - Error responses with structured code+message: file_not_found, invalid_request
  - stderr [aft] logs for unknown commands (existing)
duration: 1 step
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Outline command with dispatch wiring

**Implemented outline command with flat-to-tree symbol nesting and wired provider through dispatch.**

## What Happened

Created `src/commands/` module with outline handler. The handler deserializes `file` from request params, calls `list_symbols()` on the provider, and builds a nested `OutlineEntry` tree. Methods with `parent.is_some()` are placed under their parent using the `scope_chain` for multi-level nesting (e.g. `OuterClass → InnerClass → inner_method`). Orphan children whose parent isn't found get promoted to top level defensively.

Wired the `TreeSitterProvider` into `dispatch()` by renaming `_provider` to `provider` and adding `&dyn LanguageProvider` to the dispatch signature. Added `"outline"` arm in the command match.

Created `src/commands/zoom.rs` as an empty placeholder for T02.

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test --lib -- commands::outline::tests` — 7/7 passed ✅
- `cargo test --test integration -- test_outline` — 4/4 passed ✅
- `cargo test` — all 68 tests pass (60 unit + 8 integration), 0 regressions ✅

Slice-level verification status:
- `cargo test --lib -- commands::outline::tests` — ✅ passes
- `cargo test --lib -- commands::zoom::tests` — not yet (T02)
- `cargo test --test integration` — ✅ 8/8 pass (includes 4 new outline tests)
- `cargo build` — ✅ 0 errors, 0 warnings

## Diagnostics

Send `{"id":"1","command":"outline","file":"path/to/file.ts"}` through binary protocol. Response contains `entries` array with nested `members`. Error cases: missing `file` param → `invalid_request`, nonexistent file → `file_not_found`. Both include structured `code` + `message`.

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/commands/mod.rs` — module declarations for outline and zoom
- `src/commands/outline.rs` — OutlineEntry type, handle_outline handler, flat-to-tree nesting, 7 unit tests
- `src/commands/zoom.rs` — empty placeholder for T02
- `src/main.rs` — renamed _provider, wired through dispatch, added outline arm
- `src/lib.rs` — added `pub mod commands`
- `tests/integration/commands_test.rs` — 4 integration tests (nested structure, Python multi-level, missing file, missing param)
- `tests/integration/main.rs` — added commands_test module
- `.gsd/milestones/M001/slices/S03/tasks/T01-PLAN.md` — added Observability Impact section
