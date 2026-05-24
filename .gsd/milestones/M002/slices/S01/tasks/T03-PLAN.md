---
estimated_steps: 6
estimated_files: 8
---

# T03: remove_import + organize_imports + plugin registration

**Slice:** S01 — Import Management
**Milestone:** M002

## Description

Complete the import command surface with `remove_import` and `organize_imports`, then wire all 3 import commands into the OpenCode plugin. This closes R013 and the S01 integration loop.

`remove_import` handles two cases: removing an entire import statement (only name or last name in statement), or removing a specific name from a multi-name import while preserving the statement. `organize_imports` re-parses all imports, groups them per language convention, sorts within groups, deduplicates, and regenerates the entire import block — including Rust `use` tree merging (D045).

## Steps

1. Implement `src/commands/remove_import.rs`:
   - Params: `file` (required), `module` (required), `name` (optional — specific name to remove; if omitted, remove entire import for that module)
   - Flow: read file → parse tree → find matching import by module path → if `name` specified and import has multiple names, regenerate the import without that name; if single name or no `name` param, remove entire import line(s) → write file → validate syntax
   - Auto-backup before mutation. Return `{ file, removed, module, name?, syntax_valid?, backup_id? }`
   - Handle not-found: if module not in imports, return error `import_not_found`
   - Wire into `src/commands/mod.rs` and dispatch in `src/main.rs`

2. Implement `src/commands/organize_imports.rs`:
   - Params: `file` (required)
   - Flow: read file → parse tree → extract all imports → group by language convention → sort within groups (alphabetical by module path) → deduplicate → generate new import block text with blank line between groups → replace the original import region → write file → validate syntax
   - For Rust: merge separate `use` declarations sharing a common prefix into `use` trees (e.g., `use std::path::Path;` + `use std::path::PathBuf;` → `use std::path::{Path, PathBuf};`) — this is where D045's deferred merging happens
   - Auto-backup before mutation. Return `{ file, groups: [{name, count}], removed_duplicates, syntax_valid?, backup_id? }`
   - Wire into dispatch

3. Create `opencode-plugin-aft/src/tools/imports.ts`:
   - Define Zod schemas and tool definitions for `add_import`, `remove_import`, `organize_imports` following the D034 pattern (`const z = tool.schema`)
   - `add_import` args: file, module, names (optional string array), default_import (optional string), type_only (optional bool)
   - `remove_import` args: file, module, name (optional string)
   - `organize_imports` args: file
   - Export the tools function taking `BinaryBridge` parameter, matching existing pattern in `editing.ts`

4. Register import tools in the plugin's tool registry:
   - Update `opencode-plugin-aft/src/tools/index.ts` (or equivalent) to include import tools
   - Ensure they appear in the plugin's tool registration alongside existing tools

5. Write integration tests for remove_import and organize_imports:
   - `remove_import` removes an entire import statement (TS)
   - `remove_import` removes one name from a multi-name import, preserving the statement (TS)
   - `remove_import` returns error for non-existent module
   - `organize_imports` re-sorts a scrambled import block into correct groups (TS)
   - `organize_imports` deduplicates repeated imports
   - `organize_imports` on Python file produces isort-style grouping
   - `organize_imports` on Rust file merges common-prefix use declarations

6. Run full verification:
   - `cargo test` — 0 failures across all tests
   - `bun test` in `opencode-plugin-aft/` — plugin tests pass
   - `cargo build` — 0 warnings

## Must-Haves

- [ ] `remove_import` handles full-statement removal and partial name removal
- [ ] `remove_import` returns `import_not_found` for missing modules
- [ ] `organize_imports` re-groups, re-sorts, and deduplicates
- [ ] `organize_imports` merges Rust `use` declarations with common prefix into use trees
- [ ] Both commands auto-backup and return syntax validation
- [ ] All 3 import tools registered in OpenCode plugin with Zod schemas
- [ ] Integration tests cover remove and organize for representative languages

## Verification

- `cargo test` — 0 failures
- `cargo test --test integration` — all import integration tests pass (add + remove + organize)
- `cargo build` — 0 warnings
- `cd opencode-plugin-aft && bun test` — plugin tests pass
- S01 demo criterion: `add_import` on TS file with 3 import groups → correct group, alphabetized, deduplicated

## Observability Impact

- **New stderr signals:** `[aft] remove_import: {file}` and `[aft] organize_imports: {file}` logged on every call, matching existing `add_import` pattern
- **Structured error responses:** `remove_import` returns `code: "import_not_found"` when the specified module isn't in the file; both commands return `code: "file_not_found"` / `code: "invalid_request"` for standard failures
- **Diagnostic fields in success responses:**
  - `remove_import`: `{ removed: bool, module, name?, syntax_valid?, backup_id? }` — inspect `removed` and `syntax_valid` to verify mutation succeeded
  - `organize_imports`: `{ groups: [{name, count}], removed_duplicates, syntax_valid?, backup_id? }` — inspect `groups` to verify grouping, `removed_duplicates` for dedup count
- **How to inspect:** Send a `remove_import` or `organize_imports` request via binary protocol and check the structured response fields. All mutations create backups inspectable via `edit_history`/`restore_checkpoint`.

## Inputs

- `src/imports.rs` — T01+T02's import engine with all 6 languages
- `src/commands/add_import.rs` — reference for handler pattern and import engine usage
- `opencode-plugin-aft/src/tools/editing.ts` — reference for plugin tool definition pattern
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge interface
- `tests/integration/import_test.rs` — T01+T02's integration tests to extend

## Expected Output

- `src/commands/remove_import.rs` — remove_import handler (~120 lines)
- `src/commands/organize_imports.rs` — organize_imports handler (~150 lines)
- `src/commands/mod.rs` — updated with both modules
- `src/main.rs` — dispatch entries for both commands
- `opencode-plugin-aft/src/tools/imports.ts` — plugin tool definitions (~100 lines)
- `opencode-plugin-aft/src/tools/index.ts` — updated registration
- `tests/integration/import_test.rs` — extended with remove/organize tests (~150 additional lines)
