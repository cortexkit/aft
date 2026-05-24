# S01: Import Management

**Goal:** `add_import`, `remove_import`, and `organize_imports` commands work across all 6 languages (TS, JS, TSX, Python, Rust, Go) with correct group placement, deduplication, and alphabetization — through both the binary protocol and the OpenCode plugin.

**Demo:** Agent calls `add_import` on a TypeScript file with 3 import groups and the new import lands in the correct group, alphabetized and deduplicated — proven by integration tests across all 6 languages.

## Must-Haves

- Import detection via tree-sitter for all 6 languages (TS, JS, TSX, Python, Rust, Go)
- Per-language group classification: TS/JS (external/relative/type), Python (stdlib/third-party/local), Rust (std/external/crate), Go (stdlib/external)
- Deduplication: adding an already-present import returns success with `already_present: true`
- Alphabetical sorting within groups on insert
- `add_import` command with language-aware group placement
- `remove_import` command (remove specific import or specific name from a multi-name import)
- `organize_imports` command (re-sort, re-group, deduplicate all imports)
- All 3 commands auto-backup before mutation (existing `edit::auto_backup` pattern)
- All 3 commands return `syntax_valid` post-edit
- Plugin tool registrations for all 3 commands with Zod schemas
- Top-level imports only (D041) — document limitation in error for nested import attempts

## Proof Level

- This slice proves: contract + integration
- Real runtime required: yes (integration tests run the aft binary)
- Human/UAT required: no

## Verification

- `cargo test` — all existing tests still pass (no regressions)
- `cargo test -- import` — new unit tests for import parsing, grouping, dedup, sort per language
- `cargo test --test integration` — integration tests proving:
  - `add_import` places imports in correct group for all 6 languages
  - `add_import` deduplicates (already-present returns success without modification)
  - `add_import` alphabetizes within group
  - `remove_import` removes a specific import statement
  - `remove_import` removes one name from a multi-name import
  - `organize_imports` re-sorts and re-groups a messy import block
- `bun test` in `opencode-plugin-aft/` — plugin tool registrations type-check and round-trip through the bridge
- Integration tests verify structured error responses: `add_import` on unsupported language returns `ok: false` with `code: "invalid_request"`; `add_import` on missing file returns `ok: false` with `code: "file_not_found"`

## Observability / Diagnostics

- Runtime signals: `[aft] add_import: {file}`, `[aft] remove_import: {file}`, `[aft] organize_imports: {file}` on stderr (matching existing command logging pattern)
- Failure visibility: error responses include `code` + `message` fields (e.g. `invalid_request` for missing params, `file_not_found`, `unsupported_language`)

## Integration Closure

- Upstream surfaces consumed: `src/edit.rs` (auto_backup, validate_syntax), `src/parser.rs` (FileParser, detect_language, LangId, tree-sitter grammars), `src/context.rs` (AppContext), existing dispatch pattern in `main.rs`
- New wiring introduced: `src/imports.rs` (import analysis engine), 3 new command modules, dispatch entries, plugin tool definitions
- What remains before the milestone is truly usable end-to-end: S03 auto-format integration (imports will be correctly placed but not auto-formatted until S03)

## Tasks

- [x] **T01: Import engine core + add_import for TS/JS/TSX** `est:2h`
  - Why: The import analysis engine is the slice's core risk. TS/JS/TSX are highest-priority (D004) and share ~80% of parsing patterns. Building the shared types + first 3 languages + the `add_import` command proves the architecture end-to-end.
  - Files: `src/imports.rs`, `src/commands/add_import.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/fixtures/imports_ts.ts`, `tests/fixtures/imports_js.js`, `tests/integration/import_test.rs`, `tests/integration/main.rs`
  - Do: Build `src/imports.rs` with shared types (`ImportStatement`, `ImportGroup`, `ImportBlock`), a `LanguageImports` trait for per-language behavior, and TS/JS/TSX implementations. Key operations: find import nodes in AST, parse into structured form, classify into groups, check for duplicates, find insertion point, generate import text. Create `add_import` command handler following the `handle_*(req, ctx)` pattern. Wire into dispatch. Create import-heavy fixtures and integration tests. TS/JS group convention: external packages (no `.`/`..` prefix) → relative imports (`.`/`..` prefix). TSX shares TS logic. Type imports (`import type`) go into their respective group but sort after value imports.
  - Verify: `cargo test -- import` passes; integration tests prove add_import places TS/JS/TSX imports in correct groups, deduplicates, and alphabetizes.
  - Done when: `add_import` works for TS, JS, and TSX through the binary protocol with correct group placement, dedup, and alphabetization proven by integration tests.

- [x] **T02: Python, Rust, Go import support + add_import integration tests** `est:1.5h`
  - Why: Extends the import engine to the remaining 3 languages. Each has distinct import conventions — Python isort groups, Rust use-tree groups, Go goimports groups. Lower risk since the architecture is proven by T01.
  - Files: `src/imports.rs` (extend), `tests/fixtures/imports_py.py`, `tests/fixtures/imports_rs.rs`, `tests/fixtures/imports_go.go`, `tests/integration/import_test.rs` (extend)
  - Do: Add Python implementation (3 groups: stdlib via embedded module list, third-party, local/relative). Add Rust implementation (3 groups: std/core/alloc, external crates, crate::/self::/super::). Rust `add_import` creates new `use` declarations per D045 — merging deferred to `organize_imports`. Add Go implementation (2 groups: stdlib = no dots in path, external = dots in path). Create language-specific import fixtures. Write integration tests proving add_import for all 3 languages.
  - Verify: `cargo test -- import` passes; integration tests prove add_import works for Python, Rust, Go with correct group classification.
  - Done when: `add_import` works for all 6 languages with correct per-language group placement, dedup, and sort.

- [x] **T03: remove_import + organize_imports + plugin registration** `est:2h`
  - Why: Completes the command surface (R013) and closes the integration loop with the plugin (R009 pattern). `remove_import` handles both full-statement removal and partial name removal from multi-name imports. `organize_imports` re-sorts and re-groups the entire import block. Plugin wiring makes all 3 commands available to agents.
  - Files: `src/commands/remove_import.rs`, `src/commands/organize_imports.rs`, `src/commands/mod.rs`, `src/main.rs`, `opencode-plugin-aft/src/tools/imports.ts`, `opencode-plugin-aft/src/tools/index.ts` (or equivalent registration point), `tests/integration/import_test.rs` (extend)
  - Do: Implement `remove_import` handler — find matching import by module path, remove entire statement or specific name from multi-name import, auto-backup + validate. Implement `organize_imports` handler — parse all imports, group/sort/dedup per language convention, regenerate import block, replace in file. For Rust `organize_imports`, merge separate `use` declarations that share a common prefix into `use` trees (D045). Create plugin tool definitions with Zod schemas following D034 pattern. Add integration tests for remove and organize across representative languages (at minimum TS and Python to cover both import styles). Run `bun test` for plugin.
  - Verify: `cargo test --test integration` passes with all import tests; `bun test` in plugin passes; full S01 demo criterion met.
  - Done when: All 3 import commands work through binary protocol for all 6 languages AND through the OpenCode plugin as registered tools. `cargo test` and `bun test` pass with 0 failures.

## Files Likely Touched

- `src/imports.rs` — new import analysis engine (detection, parsing, grouping, dedup, sort)
- `src/commands/add_import.rs` — new command handler
- `src/commands/remove_import.rs` — new command handler
- `src/commands/organize_imports.rs` — new command handler
- `src/commands/mod.rs` — register new modules
- `src/main.rs` — dispatch entries for 3 new commands
- `tests/fixtures/imports_ts.ts` — TS fixture with multiple import groups
- `tests/fixtures/imports_js.js` — JS fixture with import groups
- `tests/fixtures/imports_py.py` — Python fixture with isort-style groups
- `tests/fixtures/imports_rs.rs` — Rust fixture with use declarations
- `tests/fixtures/imports_go.go` — Go fixture with import groups
- `tests/integration/import_test.rs` — integration tests for all 3 commands
- `tests/integration/main.rs` — register import_test module
- `opencode-plugin-aft/src/tools/imports.ts` — plugin tool definitions
