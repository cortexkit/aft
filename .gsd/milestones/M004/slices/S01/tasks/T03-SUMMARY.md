---
id: T03
parent: S01
milestone: M004
provides:
  - aft_move_symbol plugin tool with Zod schema in refactoring.ts tool group
  - refactoringTools(bridge) factory function for refactoring command tools
  - Bun test proving full plugin → binary → response round-trip for move_symbol
key_files:
  - opencode-plugin-aft/src/tools/refactoring.ts
  - opencode-plugin-aft/src/index.ts
  - opencode-plugin-aft/src/__tests__/tools.test.ts
key_decisions:
  - Created separate refactoring.ts tool group (not added to navigation.ts) to keep tool categories clean and ready for S02's extract_function and inline_symbol
patterns_established:
  - refactoringTools(bridge) follows same factory pattern as navigationTools, readingTools, etc.
  - move_symbol test uses navigationTools for aft_configure + refactoringTools for aft_move_symbol — tests can compose tools from different groups
observability_surfaces:
  - aft_move_symbol response includes ok, files_modified, consumers_updated, checkpoint_name for post-move inspection
  - Bun test asserts on response structure — schema drift between plugin and binary will surface as test failure
duration: 15m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T03: Plugin tool aft_move_symbol with Zod schema and bun test

**Registered `aft_move_symbol` as an OpenCode plugin tool in a new `refactoring.ts` tool group with Zod schema, and added a bun test proving the full plugin → binary → response round-trip including on-disk file verification.**

## What Happened

Created `opencode-plugin-aft/src/tools/refactoring.ts` following the exact pattern from `navigation.ts` — imports `tool.schema` as `z` (D034), defines Zod args for `file`, `symbol`, `destination`, `scope` (optional), and `dry_run` (optional), and sends `move_symbol` command via `bridge.send()`.

Registered in `index.ts` by importing `refactoringTools` and spreading into the tool object. Updated JSDoc categories.

Added a `move_symbol round-trip` test block in `tools.test.ts`. The test creates a temp directory with a source file (two exports), a consumer file (importing one), then calls `aft_configure` + `aft_move_symbol`. Asserts on `ok: true`, `files_modified >= 2`, presence of `consumers_updated` and `checkpoint_name`, and verifies on-disk mutations: symbol removed from source, added to destination, consumer import rewired from `./service` to `./utils`.

## Verification

- `npx tsc --noEmit` — clean, no type errors
- `bun test` — 40/40 pass including new `aft_move_symbol moves a function and rewires consumer import` (74ms)
- `cargo test move_symbol` — 9/9 integration tests pass (from T02)

Slice-level verification:
- ✅ `cargo test move_symbol` — all 9 integration tests pass
- ✅ `bun test` in `opencode-plugin-aft/` — all 40 tests pass including move_symbol round-trip

All slice verification checks pass. This is the final task in S01.

## Diagnostics

- Run `bun test` in `opencode-plugin-aft/` to verify plugin tool registration and round-trip
- The test creates a full temp fixture (source + consumer + destination), so failures show which specific step broke (configure, move, or file verification)
- Response JSON from `aft_move_symbol` includes `files_modified`, `consumers_updated`, `checkpoint_name` — same fields used in the integration tests

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `opencode-plugin-aft/src/tools/refactoring.ts` — new tool group with `aft_move_symbol` definition
- `opencode-plugin-aft/src/index.ts` — added refactoringTools import and registration
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — added move_symbol round-trip test with on-disk verification
- `.gsd/milestones/M004/slices/S01/tasks/T03-PLAN.md` — added Observability Impact section (pre-flight fix)
