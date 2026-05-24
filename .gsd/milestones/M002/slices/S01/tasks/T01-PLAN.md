---
estimated_steps: 6
estimated_files: 8
---

# T01: Import engine core + add_import for TS/JS/TSX

**Slice:** S01 — Import Management
**Milestone:** M002

## Description

Build the import analysis engine (`src/imports.rs`) with shared types and the TS/JS/TSX implementation, plus the `add_import` command handler. This is the slice's core risk — the per-language import parsing, group classification, deduplication, and insertion logic. TS/JS/TSX are highest-priority (D004) and share ~80% of tree-sitter patterns, making them the right first batch to prove the architecture.

The import engine provides: find import nodes in AST → parse into structured form → classify into groups → check for duplicates → find insertion point → generate import text. The `add_import` command orchestrates this into a file mutation with auto-backup and syntax validation.

## Steps

1. Create `src/imports.rs` with shared types:
   - `ImportStatement` struct (module path, imported names, kind: value/type/side-effect, byte range, raw text)
   - `ImportGroup` enum per language (e.g., for TS: External, Relative, each can have type-only variants)
   - `ImportBlock` struct (ordered list of parsed imports with their groups, overall byte range)
   - Core functions: `parse_imports(source, tree, lang) -> ImportBlock`, `find_insertion_point(block, group, name) -> usize`, `is_duplicate(block, new_import) -> bool`, `generate_import_line(lang, module_path, names, kind) -> String`
   - Language dispatch: match on `LangId` to call per-language parsing/classification
   - TS/JS/TSX grouping convention: Group 1 = external (module path doesn't start with `.`), Group 2 = relative (`.`/`..` prefix). Within each group, alphabetize by module path. Type imports sort after value imports within the same group.

2. Implement TS/JS/TSX import parsing using tree-sitter:
   - Walk the AST root's direct children (D041: top-level only) looking for `import_statement` nodes
   - Extract from each: source module (string literal child), imported names (from `import_clause` children — named imports, default import, namespace import), type-only flag (`import type`)
   - Classify into External vs Relative based on module path prefix
   - Text generation: `import { name1, name2 } from 'module';`, `import name from 'module';`, `import type { T } from 'module';`

3. Create `src/commands/add_import.rs` following the existing handler pattern:
   - Params: `file` (required), `module` (required — the module path), `names` (optional — array of names to import), `default_import` (optional — default import name), `type_only` (optional, bool)
   - Flow: read file → parse tree → parse imports → check duplicate → find insertion point → generate import line → insert at byte offset → write file → validate syntax → return result
   - Auto-backup via `edit::auto_backup` before mutation
   - Return: `{ file, added, module, group, already_present?, syntax_valid?, backup_id? }`
   - Wire into `src/commands/mod.rs` and dispatch in `src/main.rs`

4. Create import-specific test fixtures:
   - `tests/fixtures/imports_ts.ts` — TS file with 3+ import groups (external packages, relative imports, type imports), multiple imports per group
   - `tests/fixtures/imports_js.js` — JS file with external and relative imports

5. Write integration tests in `tests/integration/import_test.rs`:
   - `add_import` places a new external import into the external group (TS)
   - `add_import` places a new relative import into the relative group (TS)
   - `add_import` deduplicates (already-present module+name returns `already_present: true`)
   - `add_import` alphabetizes within group
   - `add_import` works on JS files (same logic, different grammar)
   - `add_import` on a file with no existing imports creates the import at the top
   - Register `import_test` module in `tests/integration/main.rs`

6. Verify: `cargo test` passes with 0 failures (all existing + new tests)

## Must-Haves

- [ ] `ImportStatement`, `ImportBlock` types defined with byte ranges for precise insertion
- [ ] TS/JS/TSX import node detection via tree-sitter (top-level `import_statement` only)
- [ ] Group classification: external vs relative for TS/JS/TSX
- [ ] Dedup detection: same module + same name = already present
- [ ] Alphabetical insertion within group by module path
- [ ] `add_import` command handler with auto-backup and syntax validation
- [ ] Integration tests proving correct group placement, dedup, and sort for TS/JS

## Verification

- `cargo test -- import` — all import-related unit and integration tests pass
- `cargo test --test integration` — full integration suite passes (existing + new import tests)
- `cargo test` — 0 regressions in existing tests

## Observability Impact

- **New stderr signal:** `[aft] add_import: {file}` emitted on every `add_import` invocation (matches existing command logging pattern in `write`, `edit_symbol`, etc.)
- **Structured error codes:** `invalid_request` (missing params, unsupported language), `file_not_found` (path doesn't exist), `parse_error` (tree-sitter failure) — all include `code` + `message` in JSON response
- **Future-agent inspection:** An agent debugging import behavior can send `add_import` with the same params and check `already_present`, `group`, and `syntax_valid` fields in the response to understand current import state without modifying the file
- **Failure state:** failed add_import returns `ok: false` with machine-readable `code` field; `syntax_valid: false` in success response indicates the resulting file has parse errors (import was added but file may have pre-existing syntax issues)

## Inputs

- `src/parser.rs` — `FileParser`, `detect_language()`, `LangId`, tree-sitter grammars and query infrastructure
- `src/edit.rs` — `auto_backup()`, `validate_syntax()`, `replace_byte_range()`
- `src/commands/write.rs` — reference pattern for command handler structure
- `src/protocol.rs` — `RawRequest`, `Response` types
- `src/context.rs` — `AppContext` threading pattern
- `tests/integration/helpers.rs` — `AftProcess`, `fixture_path` test infrastructure

## Expected Output

- `src/imports.rs` — import analysis engine with shared types + TS/JS/TSX implementation (~300-400 lines)
- `src/commands/add_import.rs` — command handler (~100 lines)
- `src/commands/mod.rs` — updated with `add_import` module
- `src/main.rs` — dispatch entry for `add_import`
- `tests/fixtures/imports_ts.ts` — TS import fixture
- `tests/fixtures/imports_js.js` — JS import fixture
- `tests/integration/import_test.rs` — integration tests (~150 lines)
- `tests/integration/main.rs` — updated with `import_test` module
