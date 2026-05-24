# S01: Move Symbol with Import Rewiring — UAT

**Milestone:** M004
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All verification is through automated tests (28 Rust tests + 40 bun tests) exercising the binary protocol end-to-end. No UI or human-experience surface to validate.

## Preconditions

- `cargo build` succeeds (binary available at `target/debug/aft`)
- `bun install` completed in `opencode-plugin-aft/`
- Fixture files present in `tests/fixtures/move_symbol/` (8 files including `features/` subdirectory)

## Smoke Test

Run `cargo test move_symbol_basic` — a single test that moves `formatDate` from `service.ts` to `utils.ts` and verifies the symbol is removed from source, added to destination with export, and a consumer's import path is updated. If this passes, the core pipeline works.

## Test Cases

### 1. Basic Move — symbol transferred between files

1. Create a project with `service.ts` (exports `formatDate`), `utils.ts` (existing content), and `consumer_a.ts` (imports `formatDate` from `./service`)
2. Send `configure` with the project root
3. Send `move_symbol` with `file: "service.ts"`, `symbol: "formatDate"`, `destination: "utils.ts"`
4. Read `service.ts` from disk
5. Read `utils.ts` from disk
6. Read `consumer_a.ts` from disk
7. **Expected:** `service.ts` no longer contains `formatDate`. `utils.ts` contains `export function formatDate(...)`. `consumer_a.ts` imports from `./utils` instead of `./service`. Response has `ok: true`, `files_modified >= 2`, `consumers_updated >= 1`.

### 2. Multi-consumer rewiring — 5+ files updated

1. Use the full 8-file fixture set (service.ts, utils.ts, consumer_a through consumer_f, features/consumer_e.ts)
2. Configure and move `formatDate` from `service.ts` to `utils.ts`
3. Check each consumer file on disk
4. **Expected:** consumer_a.ts — import path changed to `./utils`. consumer_b.ts — `formatDate` import split to `./utils`, other imports remain from `./service`. consumer_c.ts — import path changed, alias preserved. consumer_d.ts — unchanged (imports `DATE_FORMAT` only). consumer_f.ts — unchanged (imports `parseDate` only). features/consumer_e.ts — import path changed to `../utils` (correct parent traversal).

### 3. Aliased import preserved

1. Fixture has `consumer_c.ts` with `import { formatDate as fmtDate } from './service'`
2. Move `formatDate` from `service.ts` to `utils.ts`
3. Read `consumer_c.ts`
4. **Expected:** Import becomes `import { formatDate as fmtDate } from './utils'` — alias `fmtDate` is preserved, only the module path changes.

### 4. Dry-run returns diffs without modifying disk

1. Snapshot all file contents before the operation
2. Send `move_symbol` with `dry_run: true`
3. Compare all file contents to the pre-operation snapshot
4. **Expected:** Response has `ok: true` and `diffs` array with entries for source, destination, and consumer files. All files on disk are byte-identical to the pre-snapshot. No backup/checkpoint created.

### 5. Checkpoint safety — create and restore

1. Move `formatDate` from `service.ts` to `utils.ts`
2. Send `list_checkpoints`
3. Send `restore_checkpoint` with the returned checkpoint name
4. Read all files from disk
5. **Expected:** After move, checkpoint appears in list with name matching `move_symbol:formatDate`. After restore, all files are identical to their original content (source has `formatDate`, consumers import from `./service`).

### 6. Plugin round-trip — aft_move_symbol through OpenCode plugin

1. Create temp project with source file (two exports), consumer file (importing one), empty destination
2. Call `aft_configure` via plugin bridge
3. Call `aft_move_symbol` via plugin bridge with file, symbol, destination params
4. Read files from disk
5. **Expected:** Response includes `ok: true`, `files_modified >= 2`, `consumers_updated`, `checkpoint_name`. Source file has symbol removed. Destination file has symbol added. Consumer import path updated.

## Edge Cases

### Not-configured error guard

1. Start fresh binary (no `configure` call)
2. Send `move_symbol` command with valid file and symbol params
3. **Expected:** Response has `ok: false`, `code: "not_configured"`, message instructs calling `configure` first.

### Symbol not found

1. Configure project
2. Send `move_symbol` with `symbol: "nonexistentFunction"`
3. **Expected:** Response has `ok: false`, `code: "symbol_not_found"`.

### Non-top-level symbol rejected (D100)

1. Configure project with fixture containing `DateHelper` class with `format` method
2. Send `move_symbol` with `symbol: "format"` (a method, not top-level)
3. **Expected:** Response has `ok: false`, `code: "invalid_request"`, message contains "non-top-level" or equivalent.

### Source file not found

1. Configure project
2. Send `move_symbol` with `file: "nonexistent.ts"`
3. **Expected:** Response has `ok: false`, `code: "file_not_found"`.

### Consumer at different directory depth

1. Fixture has `features/consumer_e.ts` importing from `../service`
2. Move symbol to `utils.ts` (one directory up from consumer)
3. **Expected:** Consumer import path updated to `../utils` (correct `../` prefix maintained).

## Failure Signals

- Any `cargo test move_symbol` failure — indicates regression in the command handler or import rewriting logic
- `bun test` failure in `aft_move_symbol` test — indicates plugin schema drift or bridge communication issue
- Response `consumers_updated: 0` when consumers exist — consumer discovery or import matching is broken
- Files unchanged on disk after non-dry-run move — write pipeline failure
- Consumer import paths containing double slashes (`//`) or wrong parent traversal (`../../` when `../` expected) — relative path computation bug

## Requirements Proved By This UAT

- R028 (Move symbol with import rewiring) — test cases 1-5 prove end-to-end move with multi-file import rewiring, alias preservation, dry-run preview, and checkpoint safety
- R018 (Dry-run mode) — test case 4 proves dry_run on move_symbol returns diffs without modifying files
- R008 (Workspace-wide checkpoints) — test case 5 proves checkpoint create/restore cycle around multi-file mutation
- R009 (OpenCode plugin bridge) — test case 6 proves plugin tool registration and round-trip through binary

## Not Proven By This UAT

- Python/Rust/Go consumer import rewriting (deferred by D110 — TS/JS/TSX only)
- `require()` call rewriting (CommonJS consumers not handled)
- Barrel re-export rewriting (re-export files not detected as consumers)
- Symbol's own imports transferred to destination (not implemented)
- LSP-enhanced symbol resolution (S03 scope)

## Notes for Tester

- The `configure` command must be called before `move_symbol` — it initializes the call graph and file watcher. Every test case that exercises move_symbol success paths must configure first.
- On macOS, temp directory paths go through `/var/folders/` which is a symlink to `/private/var/folders/`. The handler canonicalizes paths to handle this (D111). If tests pass on macOS, this is working correctly.
- The fixture set intentionally includes files that should NOT be modified (consumer_d.ts, consumer_f.ts) to verify that only relevant consumers are touched.
