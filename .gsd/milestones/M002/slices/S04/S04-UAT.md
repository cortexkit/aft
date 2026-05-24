# S04: Dry-run & Transactions — UAT

**Milestone:** M002
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All success criteria are machine-verifiable via integration tests (cargo test + bun test). Dry-run diff output, file immutability, and transaction rollback are all deterministically testable.

## Preconditions

- `cargo build` succeeds with 0 warnings
- AFT binary available at `target/debug/aft`
- `bun` installed for plugin tests
- Test fixtures in `tests/fixtures/` intact

## Smoke Test

Run `cargo test dryrun_test::write_dry_run_returns_diff` — if dry-run returns a diff and the file is unchanged, the core infrastructure works.

## Test Cases

### 1. Write dry-run returns diff without modifying file

1. Create a temp file with content `const x = 1;\n`
2. Send `write` command with `dry_run: true` and new content `const x = 2;\n`
3. **Expected:** Response has `ok: true`, `dry_run: true`, `diff` containing unified diff with `-const x = 1;` and `+const x = 2;`, `syntax_valid: true`
4. Read the temp file
5. **Expected:** File still contains `const x = 1;\n` — unchanged on disk

### 2. edit_symbol dry-run (milestone acceptance scenario)

1. Create a TypeScript file with `function greet() { return "hello"; }`
2. Send `edit_symbol` with `symbol: "greet"`, `operation: "replace"`, replacement code, `dry_run: true`
3. **Expected:** Response has `dry_run: true`, non-empty `diff` field, `syntax_valid: true`
4. Read the file
5. **Expected:** File unchanged — original function body intact

### 3. edit_match dry-run

1. Create a file with `const name = "old";`
2. Send `edit_match` with `match: "old"`, `replacement: "new"`, `dry_run: true`
3. **Expected:** Response has `dry_run: true`, diff showing `"old"` → `"new"`, file unchanged

### 4. Batch dry-run shows combined diff

1. Create a file with two lines: `const a = 1;\nconst b = 2;\n`
2. Send `batch` with two edit_match operations changing both values, `dry_run: true`
3. **Expected:** Single diff containing both changes, file unchanged

### 5. add_import dry-run

1. Create a TypeScript file with an existing import
2. Send `add_import` with a new import, `dry_run: true`
3. **Expected:** Diff shows new import line added, file unchanged

### 6. Dry-run creates no backup entries

1. Create a file and send a dry-run write
2. Send `edit_history` for the same file
3. **Expected:** Empty history — no backup entries created

### 7. Dry-run syntax validation for broken content

1. Create a TypeScript file
2. Send `write` with `dry_run: true` and syntactically broken content (`function {{{ broken`)
3. **Expected:** Response has `syntax_valid: false`, diff present, file unchanged

### 8. Dry-run empty diff for no-op

1. Create a file with content `const x = 1;\n`
2. Send `write` with `dry_run: true` and identical content
3. **Expected:** Response has empty `diff` string

### 9. Transaction success — 3 files atomically modified

1. Send `transaction` with 3 `write` operations targeting 3 different temp files
2. **Expected:** Response has `ok: true`, `files_modified: 3`, `results` array with per-file `syntax_valid` and `formatted`
3. All 3 files exist on disk with correct content

### 10. Transaction rollback on syntax error (milestone acceptance scenario)

1. Create 2 existing TypeScript files with valid content
2. Send `transaction` with 3 operations: write valid content to file 1, write valid content to file 2, write broken TypeScript (`function {{broken`) to file 3
3. **Expected:** Response has `ok: false`, `code: "transaction_failed"`, `failed_operation: 2` (0-indexed), `rolled_back` array listing all 3 files
4. Read all 3 files
5. **Expected:** File 1 and file 2 restored to original content, file 3 either deleted (if new) or restored

### 11. Transaction rollback deletes new files

1. Create one existing file
2. Send `transaction` with 2 operations: write valid content to existing file, write broken content to a new file
3. **Expected:** Rollback deletes the new file (doesn't exist on disk), restores existing file to original content
4. `rolled_back` array shows `action: "restored"` for existing and `action: "deleted"` for new

### 12. Transaction with edit_match operations

1. Create a file with `const x = "old";`
2. Send `transaction` with an `edit_match` operation: match `"old"`, replace with `"new"`
3. **Expected:** Success, file now contains `const x = "new";`

### 13. Transaction dry-run

1. Send `transaction` with `dry_run: true` and multiple write operations
2. **Expected:** Response has `diffs` array with per-file `{ file, diff, syntax_valid }`, no files created or modified on disk

### 14. Transaction empty operations rejected

1. Send `transaction` with `operations: []`
2. **Expected:** Error response with message indicating empty operations

### 15. Plugin dry-run round-trip

1. In bun test, call the `write` tool with `dry_run: true` through the plugin bridge
2. **Expected:** Response includes `dry_run: true` and `diff` field

### 16. Plugin transaction round-trip

1. In bun test, call the `transaction` tool with 2 write operations through the plugin bridge
2. **Expected:** Response includes `ok: true` and `files_modified: 2`

### 17. Plugin transaction rollback round-trip

1. In bun test, call the `transaction` tool with one valid and one invalid write through the plugin bridge
2. **Expected:** Response includes `code: "transaction_failed"` and `rolled_back` array

## Edge Cases

### Dry-run on unsupported language

1. Create a file with `.xyz` extension
2. Send `write` with `dry_run: true`
3. **Expected:** `syntax_valid: null` (can't validate unsupported language), diff still generated

### Transaction with duplicate files

1. Send `transaction` with 2 operations targeting the same file
2. **Expected:** Both operations apply in order (second overwrites first), single snapshot taken

### Transaction where write failure (not syntax) triggers rollback

1. Send `transaction` with an `edit_match` where the match string doesn't exist in the file
2. **Expected:** Rollback triggered, error identifies the failed operation index

## Failure Signals

- Any test case where `dry_run: true` is sent but the file is modified on disk
- Any test case where dry-run response lacks the `diff` or `syntax_valid` fields
- Transaction rollback that leaves files in a partially-modified state
- `edit_history` showing backup entries after a dry-run operation
- Missing `rolled_back` array in transaction error response
- Plugin tests failing to pass `dry_run` through to the binary

## Requirements Proved By This UAT

- R018 — Dry-run mode on all mutations: test cases 1–8 prove all mutation commands accept `dry_run: true` and return diff without side effects
- R019 — Multi-file atomic transactions: test cases 9–14 prove atomic application and full rollback on failure

## Not Proven By This UAT

- Dry-run does not prove formatted output preview (D071 — deferred)
- Transaction does not prove atomicity for commands beyond write and edit_match (D072 — scoped)
- No load/stress testing of transaction with many files

## Notes for Tester

All test cases map 1:1 to existing integration tests in `dryrun_test.rs` and `transaction_test.rs`, plus bun tests in `tools.test.ts`. Running `cargo test` and `bun test` exercises all cases. The milestone acceptance scenarios (cases 2 and 10) are the critical paths.
