---
estimated_steps: 5
estimated_files: 7
---

# T03: Plugin registration for dry_run param and transaction tool

**Slice:** S04 — Dry-run & Transactions
**Milestone:** M002

## Description

Register dry-run support and the transaction command in the OpenCode plugin. Add `dry_run` as an optional boolean param to all 12 existing mutation tool schemas. Create a new `transaction.ts` tool definition. Wire into plugin index. Add bun tests for round-trip verification.

## Steps

1. In `opencode-plugin-aft/src/tools/editing.ts`: add `dry_run: z.boolean().optional().describe("Preview the edit as a unified diff without modifying the file")` to the `args` of all 4 editing tools (write, edit_symbol, edit_match, batch). Pass `dry_run` through to `bridge.send` when provided.
2. In `opencode-plugin-aft/src/tools/imports.ts`: add the same `dry_run` param to all 3 import tools (add_import, remove_import, organize_imports). Pass through when provided.
3. In `opencode-plugin-aft/src/tools/structure.ts`: add the same `dry_run` param to all 5 structure tools (add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags). Pass through when provided.
4. Create `opencode-plugin-aft/src/tools/transaction.ts`: define the `transaction` tool with schema for `operations` array (each with `file`, `command` enum of "write"/"edit_match", optional `content`, optional `match`, optional `replacement`), optional `dry_run` boolean, and optional `validate` enum. Use `const z = tool.schema;` per D034.
5. In `opencode-plugin-aft/src/index.ts`: import `transactionTools` from `./tools/transaction.js` and spread into the tool object. Update the tool categories comment.
6. In `opencode-plugin-aft/src/__tests__/tools.test.ts`: add tests:
   - `write dry_run returns diff without modifying file` — call write tool with dry_run:true, verify response has diff and dry_run:true, verify file unchanged
   - `transaction success` — call transaction tool with 2 write operations, verify both files modified
   - `transaction rollback on syntax error` — call transaction with a file containing broken syntax, verify rollback

## Must-Haves

- [ ] All 12 mutation tools accept `dry_run` optional param
- [ ] `dry_run` param passed through to bridge.send when provided
- [ ] Transaction tool registered with correct Zod schema
- [ ] Transaction tool imported and spread in index.ts
- [ ] Bun tests pass for dry-run and transaction round-trips
- [ ] Uses `const z = tool.schema;` not direct zod import (D034)

## Verification

- `bun test` in `opencode-plugin-aft/` — all tests pass including new dry-run and transaction tests
- `bun run build` (if applicable) — no type errors

## Inputs

- Existing tool files in `opencode-plugin-aft/src/tools/` — patterns for param passing and schema definition
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — existing test patterns
- Binary-side dry-run and transaction implementations from T01 and T02

## Expected Output

- `opencode-plugin-aft/src/tools/editing.ts` — `dry_run` param added to 4 tools
- `opencode-plugin-aft/src/tools/imports.ts` — `dry_run` param added to 3 tools
- `opencode-plugin-aft/src/tools/structure.ts` — `dry_run` param added to 5 tools
- `opencode-plugin-aft/src/tools/transaction.ts` — new transaction tool definition
- `opencode-plugin-aft/src/index.ts` — transaction tools imported and spread
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — 3 new round-trip tests

## Observability Impact

- **Schema validation:** Each mutation tool's Zod schema now includes `dry_run` — agent SDKs that introspect tool schemas will see the parameter. If `dry_run` is passed to a tool, the binary response includes `dry_run: true`, `diff`, and `syntax_valid` fields, confirming the dry-run path was taken.
- **Transaction tool:** Registered as a new tool in the plugin. On success: `ok`, `files_modified`, `results` array. On failure: `failed_operation` index, `rolled_back` array with per-file `{ file, action }`. On dry-run: `dry_run: true`, `diffs` array with per-file `{ file, diff, syntax_valid }`.
- **Future agent inspection:** To verify dry-run is wired, call any mutation tool with `dry_run: true` and confirm the response has `dry_run: true` and a `diff` field. To verify transaction is registered, check `index.ts` tool spread or call with an operations array.
- **Failure visibility:** If `dry_run` param is omitted from a tool schema, the binary ignores it (no error) but the response won't contain `dry_run: true` — silent failure. Tests catch this by asserting `dry_run: true` in the response.
