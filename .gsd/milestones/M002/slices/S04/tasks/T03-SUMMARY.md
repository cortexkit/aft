---
id: T03
parent: S04
milestone: M002
provides:
  - dry_run param registered on all 12 mutation tool schemas in OpenCode plugin
  - transaction tool definition with Zod schema and bridge wiring
  - 3 new bun round-trip tests (dry-run, transaction success, transaction rollback)
key_files:
  - opencode-plugin-aft/src/tools/editing.ts
  - opencode-plugin-aft/src/tools/imports.ts
  - opencode-plugin-aft/src/tools/structure.ts
  - opencode-plugin-aft/src/tools/transaction.ts
  - opencode-plugin-aft/src/index.ts
  - opencode-plugin-aft/src/__tests__/tools.test.ts
key_decisions:
  - "Transaction error response from binary is flat (ok:false, code, message, rolled_back at top level) — tests match that shape rather than nested error object"
patterns_established:
  - "dry_run param pattern: add to args schema, pass through with `if (args.dry_run !== undefined) params.dry_run = args.dry_run` — consistent across all 12 mutation tools"
  - "transactionOperation Zod object schema with command enum discriminator — matches binary-side ParsedOp structure"
observability_surfaces:
  - "Any mutation tool called with dry_run:true returns {ok, dry_run:true, diff, syntax_valid} — confirms dry-run path was taken"
  - "Transaction tool success: {ok, files_modified, results:[{file, syntax_valid, formatted}]}; failure: {ok:false, code:'transaction_failed', failed_operation, rolled_back:[{file,action}]}"
duration: 15m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T03: Plugin registration for dry_run param and transaction tool

**Added `dry_run` param to all 12 mutation tool schemas, created `transaction` tool definition, wired into plugin index, and added 3 round-trip tests.**

## What Happened

Added `dry_run: z.boolean().optional()` to the args of all 12 mutation tools across 3 files (editing.ts: 4 tools, imports.ts: 3 tools, structure.ts: 5 tools). Each tool passes `dry_run` through to `bridge.send` when provided, following the existing conditional-passthrough pattern used for `validate`.

Created `transaction.ts` with a `transactionOperation` Zod object schema (file, command enum, optional content/match/replacement) and the `transaction` tool definition. Uses `const z = tool.schema` per D034.

Wired `transactionTools` into `index.ts` with import and spread, updated tool categories comment.

Added 3 tests: write dry-run (verifies diff returned, file unchanged), transaction success (2-file write, both files created), transaction rollback (syntax error triggers rollback, existing file restored).

## Verification

- `bun test` in `opencode-plugin-aft/`: 39 passed, 0 failed, 157 assertions
- `cargo test`: 119 passed, 0 failed
- `cargo build`: 0 warnings

Slice-level verification status:
- ✅ `cargo test` — all existing + dry-run + transaction integration tests pass
- ✅ `cargo build` — 0 warnings
- ✅ `bun test` — plugin tests pass including new dry-run and transaction round-trips

## Diagnostics

- Call any mutation tool with `dry_run: true` — response should have `dry_run: true` and `diff` field
- Call `transaction` with an operations array — success returns `files_modified` count, failure returns `code: "transaction_failed"` with `rolled_back` array
- If `dry_run` param is silently ignored (no `dry_run: true` in response), the schema or passthrough is missing

## Deviations

Transaction rollback test initially asserted `result.error.code` (nested error object), but the binary returns flat error responses (`result.code`, `result.rolled_back` at top level). Fixed test assertions to match actual binary response shape.

## Known Issues

None.

## Files Created/Modified

- `opencode-plugin-aft/src/tools/editing.ts` — `dry_run` param added to write, edit_symbol, edit_match, batch
- `opencode-plugin-aft/src/tools/imports.ts` — `dry_run` param added to add_import, remove_import, organize_imports
- `opencode-plugin-aft/src/tools/structure.ts` — `dry_run` param added to add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags
- `opencode-plugin-aft/src/tools/transaction.ts` — new transaction tool definition with Zod schema
- `opencode-plugin-aft/src/index.ts` — imported and spread transactionTools, updated tool categories comment
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — 3 new tests: dry-run round-trip, transaction success, transaction rollback
- `.gsd/milestones/M002/slices/S04/tasks/T03-PLAN.md` — added Observability Impact section
