---
id: T03
parent: S02
milestone: M002
provides:
  - structureTools plugin registration for 5 S02 commands (add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags)
key_files:
  - opencode-plugin-aft/src/tools/structure.ts
  - opencode-plugin-aft/src/index.ts
  - opencode-plugin-aft/src/__tests__/structure.test.ts
key_decisions:
  - "add_member position schema uses z.enum(['first','last']).or(z.string()) to support both enum values and before:/after: string patterns in a single optional field"
patterns_established:
  - "Structure tool registration follows the same D034 pattern as imports.ts: const z = tool.schema, Record<string, ToolDefinition> return, params-building execute functions"
observability_surfaces:
  - "Plugin layer is pass-through — all stderr logging and structured error codes originate from binary (T01/T02). Schema validation errors from Zod surface before reaching binary."
duration: 15m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T03: Plugin tool registrations for all 5 commands

**Registered all 5 S02 structure commands as OpenCode plugin tools with Zod schemas and 14 bun tests covering registration, schema shape, round-trip execution, and error responses.**

## What Happened

Created `structure.ts` exporting `structureTools(bridge)` with 5 tool definitions following the established D034 pattern from `imports.ts`. Each tool has descriptive descriptions, typed Zod schemas (required params enforced, optional params correctly typed), and execute functions that build params and call `bridge.send()`.

Wired `structureTools` into `index.ts` alongside existing tool categories. Updated the JSDoc tool category listing.

Wrote `structure.test.ts` with two test suites: registration tests (5 tests verifying tool count, descriptions, args shape) and round-trip tests (9 tests exercising each command through the binary bridge with real file operations, including position variants, custom catch_body, and error code assertions for scope_not_found/target_not_found).

## Verification

- `bun test` — 36 tests pass across 4 files (14 new structure tests + 22 existing), 0 failures
- `cargo test` — 154 unit tests + 95 integration tests pass, 0 failures
- `cargo build 2>&1 | grep -c warning` → 0

Slice-level verification (all checks pass — this is the final task):
- ✅ `cargo build` — 0 warnings
- ✅ `cargo test` — all pass including member and structure integration tests
- ✅ `cargo test -- member` — 14 add_member tests pass
- ✅ `cargo test -- structure` — 21 compound operation tests pass
- ✅ `bun test` — plugin schema registration tests for all 5 commands pass
- ✅ Error responses include structured `code` field — verified by integration test assertions

## Diagnostics

- Plugin tools are discoverable through the OpenCode tool registry — all 5 structure commands appear alongside existing reading/editing/safety/imports tools
- Schema validation errors (missing required params) surfaced by Zod before reaching the binary
- Binary-level errors pass through as JSON: `scope_not_found` (with available scopes), `target_not_found` (with available targets), `field_not_found` (with available fields)
- All runtime stderr logging (`[aft] add_member: {file}`, etc.) originates from binary layer (T01/T02)

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `opencode-plugin-aft/src/tools/structure.ts` — 5 tool definitions with Zod schemas for add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags
- `opencode-plugin-aft/src/index.ts` — wired structureTools import and spread, updated tool category JSDoc
- `opencode-plugin-aft/src/__tests__/structure.test.ts` — 14 tests covering registration, schema shape, round-trips, and error responses
- `.gsd/milestones/M002/slices/S02/tasks/T03-PLAN.md` — added Observability Impact section (pre-flight fix)
