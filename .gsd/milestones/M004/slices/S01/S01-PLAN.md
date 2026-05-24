# S01: Move Symbol with Import Rewiring

**Goal:** `move_symbol` command moves a top-level symbol from one file to another, updates all import statements across the workspace, and supports dry_run mode — verified by integration tests with multi-file fixtures spanning 5+ consumer files.
**Demo:** Agent sends a single `move_symbol` command to move a function from `service.ts` to `utils.ts`. The function is removed from the source, added to the destination with correct export, and all 5+ consumer files that imported it from the source now import it from the destination — including one consumer that used an aliased import.

## Must-Haves

- `move_symbol` command handler following `handle_*(req, ctx)` pattern (D026) in `src/commands/move_symbol.rs`
- Relative path computation utility that produces correct `./` and `../` import paths between files at arbitrary directory depths
- Auto-checkpoint before any file modifications (D105) via named checkpoint including the moved symbol name
- Consumer discovery via `callers_of()` from the call graph engine (M003)
- Import rewiring: for each consumer file, update the import module path from source to destination, handling named imports, default imports, and aliased imports
- Source file cleanup: remove the symbol declaration and its export; remove the import statement if no other symbols remain in it
- Destination file: add the symbol with export, add necessary imports that the symbol itself depends on
- All file writes go through `write_format_validate()` (D046, D066)
- `dry_run: true` returns multi-file diff preview without modifying disk (D071)
- Restricted to top-level symbols only (D100) — methods and class members return an error
- Requires `configure` to be called first (call graph dependency) — returns `not_configured` error otherwise
- Plugin tool `aft_move_symbol` registered with Zod schema and tested via bun test
- Integration tests proving import rewiring across 5+ consumer files including aliased imports

## Proof Level

- This slice proves: contract + integration (command protocol through the binary, multi-file mutation with import rewiring verified end-to-end)
- Real runtime required: yes (binary spawned, fixture files on disk, imports parsed and rewritten)
- Human/UAT required: no

## Verification

- `cargo test move_symbol` — integration tests proving:
  - Basic move: symbol removed from source, added to destination, consumer imports updated
  - 5+ consumer files all rewired correctly (different directory depths)
  - Aliased import preserved after rewiring (`import { X as Y }` → path changes, alias stays)
  - Dry-run returns multi-file diff, files unchanged on disk
  - `not_configured` error when call graph not initialized
  - `symbol_not_found` error for nonexistent symbol
  - Method/class member rejected with appropriate error (D100)
  - Checkpoint created before mutations, restorable on failure
- `bun test` in `opencode-plugin-aft/` — plugin round-trip test for `aft_move_symbol`

## Observability / Diagnostics

- Runtime signals: response includes `files_modified` count, `consumers_updated` count, `checkpoint_name` for rollback identification, per-file `syntax_valid` and `formatted` status
- Inspection surfaces: `list_checkpoints` command shows the auto-created checkpoint; `callers` command can verify consumer list pre/post move
- Failure visibility: on partial failure, checkpoint enables full rollback; error response includes `failed_file` and `rolled_back` file list
- Redaction constraints: none

## Integration Closure

- Upstream surfaces consumed: `CallGraph::callers_of()` (M003), `imports::parse_imports()` + `find_insertion_point()` + `generate_import_line()` + `is_duplicate()` (M002), `edit::write_format_validate()` (M002), `CheckpointStore::create()` (M001), `LanguageProvider::resolve_symbol()` + `list_symbols()` (M001)
- New wiring introduced in this slice: `move_symbol` dispatch entry in `main.rs`, `move_symbol` module in `commands/mod.rs`, `aft_move_symbol` tool in plugin, new `refactoring.ts` tool group
- What remains before the milestone is truly usable end-to-end: S02 (extract_function, inline_symbol), S03 (LSP-enhanced resolution)

## Tasks

- [x] **T01: Implement move_symbol command handler with relative path computation** `est:3h`
  - Why: Core implementation of R028 — the command handler that orchestrates symbol extraction, multi-file import rewiring, checkpoint safety, and dry_run support
  - Files: `src/commands/move_symbol.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/fixtures/move_symbol/` (6-8 fixture files)
  - Do: Build `handle_move_symbol` following the `handle_*(req, ctx)` pattern. Implement: (1) param extraction (file, symbol, destination, scope), (2) call graph `not_configured` guard, (3) symbol resolution via `resolve_symbol` + top-level check (D100), (4) relative path computation utility for import rewriting, (5) auto-checkpoint (D105), (6) consumer discovery via `callers_of`, (7) source file mutation (remove symbol + clean up empty exports/imports), (8) destination file mutation (append symbol with export), (9) consumer file import path rewriting (parse imports → find matching import → recompute path → regenerate statement), (10) all writes through `write_format_validate`, (11) dry_run multi-file diff path, (12) rollback on any write failure. Wire into `dispatch()` and `commands/mod.rs`. Create multi-file fixture set with 5+ consumer files at different directory depths including aliased imports.
  - Verify: `cargo build` succeeds; fixture files are well-formed (parseable by tree-sitter)
  - Done when: `handle_move_symbol` compiles, is wired into dispatch, and the fixture set is in place for T02 to test against

- [x] **T02: Integration tests for move_symbol through binary protocol** `est:2h`
  - Why: Proves R028 acceptance criteria — all import statements across the workspace are updated, no broken references, verified through the binary protocol with multi-file fixtures
  - Files: `tests/integration/move_symbol_test.rs`, `tests/integration/main.rs`
  - Do: Write integration tests using `AftProcess` + temp dir pattern (copy fixtures, configure, execute move_symbol, verify all files). Tests: (1) basic move — symbol in source removed, appears in destination with export, (2) 5+ consumer imports all updated with correct relative paths, (3) aliased import preserved (`import { X as Y }` keeps alias), (4) dry_run returns diffs for all affected files without modifying disk, (5) not_configured error guard, (6) symbol_not_found error, (7) non-top-level symbol rejection (method inside class), (8) checkpoint auto-created and restorable. Register test module in `tests/integration/main.rs`.
  - Verify: `cargo test move_symbol` — all 8+ tests pass
  - Done when: All integration tests pass, proving move_symbol updates imports in 5+ consumer files including aliased imports

- [x] **T03: Plugin tool aft_move_symbol with Zod schema and bun test** `est:45m`
  - Why: Agents access move_symbol through the plugin — this wires the tool registration with proper Zod schema and proves the round-trip works
  - Files: `opencode-plugin-aft/src/tools/refactoring.ts`, `opencode-plugin-aft/src/index.ts`, `opencode-plugin-aft/src/__tests__/tools.test.ts`
  - Do: Create `refactoring.ts` tool group with `aft_move_symbol` tool definition (Zod schema for file, symbol, destination, scope, dry_run params). Import and spread into index.ts plugin registration. Add bun test that spawns bridge, creates a temp fixture, and verifies move_symbol round-trip through the plugin.
  - Verify: `cd opencode-plugin-aft && bun test` — new move_symbol test passes alongside existing tests
  - Done when: `aft_move_symbol` appears in plugin tool list, bun test proves round-trip through plugin → binary → response

## Files Likely Touched

- `src/commands/move_symbol.rs` (new — command handler)
- `src/commands/mod.rs` (add module declaration)
- `src/main.rs` (add dispatch entry)
- `tests/fixtures/move_symbol/` (new — multi-file fixture set)
- `tests/integration/move_symbol_test.rs` (new — integration tests)
- `tests/integration/main.rs` (register test module)
- `opencode-plugin-aft/src/tools/refactoring.ts` (new — plugin tool group)
- `opencode-plugin-aft/src/index.ts` (import and register refactoring tools)
- `opencode-plugin-aft/src/__tests__/tools.test.ts` (add move_symbol round-trip test)
