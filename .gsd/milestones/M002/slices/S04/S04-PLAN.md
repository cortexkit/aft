# S04: Dry-run & Transactions

**Goal:** Every mutation command supports `dry_run: true` returning a unified diff without modifying disk, and a new `transaction` command applies multi-file edits atomically with rollback on failure.
**Demo:** Agent dry-runs an `edit_symbol` and receives a correct unified diff without the file being modified; agent applies a 3-file transaction where the third file has a syntax error and all 3 files are rolled back to pre-transaction state.

## Must-Haves

- `similar` crate added for unified diff generation (D044)
- `validate_syntax_str()` function validates syntax from an in-memory string without touching disk
- `dry_run_diff()` function generates unified diff and validates syntax in-memory
- All 12 mutation commands accept `dry_run: true` and return `{ ok, dry_run, diff, syntax_valid }` without modifying the file, creating backups, or invoking formatters
- Batch dry-run returns the combined diff of all edits
- `transaction` command accepts an array of operations (`write` and `edit_match`), snapshots all files, applies all, validates all, rolls back everything on any failure
- Transaction rollback triggers on `syntax_valid == Some(false)` for any file
- Transaction handles new files (rollback deletes them instead of restoring)
- Transaction rollback order is reverse of application order
- Plugin adds `dry_run` optional param to all 12 mutation tool schemas
- Plugin adds `transaction` tool definition
- Bun tests verify plugin round-trips for dry-run and transaction

## Proof Level

- This slice proves: contract + integration (binary protocol + plugin round-trips)
- Real runtime required: yes (binary spawned for integration tests)
- Human/UAT required: no

## Verification

- `cargo test` — all existing tests pass + new dry-run and transaction integration tests in `tests/integration/dryrun_test.rs` and `tests/integration/transaction_test.rs`
- `cargo build` — 0 warnings
- `bun test` in `opencode-plugin-aft/` — plugin tests pass including new dry-run and transaction round-trips
- Dry-run tests prove: diff output is correct unified diff format, file is unchanged on disk, no backup created, syntax_valid reflects proposed content
- Transaction tests prove: success path (3 files modified atomically), rollback on syntax error (all files restored), rollback on new file (file deleted), per-file result reporting
- Milestone acceptance scenarios verified:
  - `edit_symbol` with `dry_run: true` → unified diff returned, file unchanged on disk
  - `transaction` across 3 files, third fails syntax → all 3 rolled back

## Observability / Diagnostics

- Runtime signals: transaction response includes `failed_operation` index and `rolled_back` file list on failure
- Inspection surfaces: dry-run diff output is standard unified diff (parseable by agents); transaction error response identifies which operation failed
- Failure visibility: transaction rollback reports which files were restored vs which were new (deleted)

## Integration Closure

- Upstream surfaces consumed: `edit::write_format_validate()`, `BackupStore::snapshot()/restore_latest()`, `parser::detect_language()/grammar_for()`, all 12 mutation command handlers
- New wiring introduced in this slice: `transaction` command handler + dispatch arm, `dry_run` param extraction in all mutation handlers, plugin `transaction` tool + `dry_run` param on all tools
- What remains before the milestone is truly usable end-to-end: nothing — S04 is the final slice

## Tasks

- [x] **T01: Add dry-run infrastructure and wire into all 12 mutation handlers** `est:2h`
  - Why: R018 requires every mutation command to support `dry_run: true` with unified diff preview. This is the cross-cutting foundation — touch all handlers with a consistent 5-line pattern.
  - Files: `Cargo.toml`, `src/edit.rs`, `src/commands/write.rs`, `src/commands/edit_symbol.rs`, `src/commands/edit_match.rs`, `src/commands/batch.rs`, `src/commands/add_import.rs`, `src/commands/remove_import.rs`, `src/commands/organize_imports.rs`, `src/commands/add_member.rs`, `src/commands/add_derive.rs`, `src/commands/add_decorator.rs`, `src/commands/add_struct_tags.rs`, `src/commands/wrap_try_catch.rs`, `tests/integration/dryrun_test.rs`, `tests/integration/main.rs`
  - Do: Add `similar` dep to Cargo.toml. In `edit.rs`: add `validate_syntax_str(content, path)` that parses an in-memory string using `detect_language` + `grammar_for` + `parser.parse(content, None)`, and `dry_run_diff(original, proposed, path)` returning `DryRunResult { diff, syntax_valid }` using `similar::TextDiff`. In each of the 12 mutation handlers: extract `dry_run` bool from params, add early return after computing `new_source` that calls `dry_run_diff` and returns `{ ok, dry_run, diff, syntax_valid }` — skipping `auto_backup` and `write_format_validate`. Batch dry-run uses the final combined content. Write integration tests covering write, edit_symbol, edit_match, batch, add_import, and add_member dry-run paths.
  - Verify: `cargo test` passes (all existing + new dryrun_test), `cargo build` 0 warnings
  - Done when: all 12 handlers accept `dry_run: true`, return unified diff, leave files unchanged, create no backups

- [x] **T02: Add transaction command with multi-file atomicity and rollback** `est:1.5h`
  - Why: R019 requires atomic multi-file edits with full rollback on failure. New command handler following established patterns.
  - Files: `src/commands/transaction.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/integration/transaction_test.rs`, `tests/integration/main.rs`
  - Do: Create `transaction.rs` handler accepting `{ operations: [{ file, command, content?, match?, replacement? }] }`. Implement phase model: parse → snapshot all → apply each (write or edit_match via `write_format_validate`) → check syntax_valid on all → rollback all on any failure. Track new vs existing files — rollback deletes new files, restores existing from backup. Rollback in reverse order. Support `dry_run: true` on transaction (per-file diffs, no disk writes). Wire dispatch in `main.rs`, add `pub mod transaction` in `mod.rs`. Write integration tests: success path (3 files), rollback on syntax error, rollback with new file creation, dry-run transaction.
  - Verify: `cargo test` passes including transaction tests, the milestone 3-file rollback acceptance scenario passes
  - Done when: transaction applies multi-file edits atomically, rolls back all files when any has syntax error, handles new file cleanup

- [x] **T03: Plugin registration for dry_run param and transaction tool** `est:1h`
  - Why: Agents access AFT through the OpenCode plugin — dry-run and transaction must be registered as plugin tools with Zod schemas.
  - Files: `opencode-plugin-aft/src/tools/editing.ts`, `opencode-plugin-aft/src/tools/imports.ts`, `opencode-plugin-aft/src/tools/structure.ts`, `opencode-plugin-aft/src/tools/transaction.ts`, `opencode-plugin-aft/src/index.ts`, `opencode-plugin-aft/src/__tests__/tools.test.ts`
  - Do: Add `dry_run: z.boolean().optional()` param to all 12 mutation tools across editing.ts (4), imports.ts (3), structure.ts (5). Pass `dry_run` through to bridge.send when provided. Create `transaction.ts` with the transaction tool schema. Import and spread in index.ts. Add bun tests: dry-run round-trip (write with dry_run returns diff, file unchanged) and transaction round-trip (success + rollback paths).
  - Verify: `bun test` passes in `opencode-plugin-aft/`
  - Done when: all plugin tools accept `dry_run`, transaction tool registered, bun tests pass

## Files Likely Touched

- `Cargo.toml`
- `src/edit.rs`
- `src/commands/write.rs`
- `src/commands/edit_symbol.rs`
- `src/commands/edit_match.rs`
- `src/commands/batch.rs`
- `src/commands/add_import.rs`
- `src/commands/remove_import.rs`
- `src/commands/organize_imports.rs`
- `src/commands/add_member.rs`
- `src/commands/add_derive.rs`
- `src/commands/add_decorator.rs`
- `src/commands/add_struct_tags.rs`
- `src/commands/wrap_try_catch.rs`
- `src/commands/transaction.rs`
- `src/commands/mod.rs`
- `src/main.rs`
- `tests/integration/dryrun_test.rs`
- `tests/integration/transaction_test.rs`
- `tests/integration/main.rs`
- `opencode-plugin-aft/src/tools/editing.ts`
- `opencode-plugin-aft/src/tools/imports.ts`
- `opencode-plugin-aft/src/tools/structure.ts`
- `opencode-plugin-aft/src/tools/transaction.ts`
- `opencode-plugin-aft/src/index.ts`
- `opencode-plugin-aft/src/__tests__/tools.test.ts`
