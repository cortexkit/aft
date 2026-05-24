---
id: T03
parent: S01
milestone: M002
provides:
  - "remove_import command handler with full-statement and partial-name removal"
  - "organize_imports command handler with re-grouping, sorting, dedup, and Rust use-tree merging"
  - "All 3 import tools (add_import, remove_import, organize_imports) registered in OpenCode plugin with Zod schemas"
  - "7 new integration tests covering remove and organize across TS, Python, and Rust"
key_files:
  - src/commands/remove_import.rs
  - src/commands/organize_imports.rs
  - src/commands/mod.rs
  - src/main.rs
  - opencode-plugin-aft/src/tools/imports.ts
  - opencode-plugin-aft/src/index.ts
  - tests/integration/import_test.rs
key_decisions:
  - "D055: organize_imports Rust merging groups by (prefix, kind, is_pub) tuple — only merges use declarations that share prefix AND visibility AND kind. Single-item results keep flat syntax (use std::collections::HashMap;) while multi-item results use tree syntax (use std::path::{Path, PathBuf};)"
  - "D056: remove_import with name param that matches the only name in a multi-name import removes the entire statement (including trailing newline), same as no-name mode"
patterns_established:
  - "Import command handlers follow same structure: extract params → validate file/lang → parse_file_imports → auto_backup → mutate → write → validate_syntax → respond"
  - "Plugin tool registration: each tool category gets its own file (reading.ts, editing.ts, safety.ts, imports.ts) with a function taking BinaryBridge and returning Record<string, ToolDefinition>"
observability_surfaces:
  - "stderr: [aft] remove_import: {file} — logged on every call"
  - "stderr: [aft] organize_imports: {file} — logged on every call"
  - "Error response: code: import_not_found for remove_import on missing module"
  - "Response field: removed_duplicates (organize_imports) and groups (organize_imports) for diagnostic inspection"
duration: 1 task
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T03: remove_import + organize_imports + plugin registration

**Completed the import command surface with `remove_import` and `organize_imports` handlers, wired all 3 import commands into the OpenCode plugin with Zod schemas, and proved everything through 7 new integration tests.**

## What Happened

Built `remove_import` with two modes: entire-statement removal (when no `name` param or last name) and partial-name removal (regenerates the import line without the target name). Uses reverse-offset editing to handle multiple matching imports safely.

Built `organize_imports` which parses all imports, groups by language convention (Stdlib→External→Internal), sorts alphabetically within groups, deduplicates, and regenerates the import block with blank-line separators between groups. For Rust specifically, implements D045's deferred use-tree merging: separate `use` declarations sharing a common prefix (e.g. `use std::path::Path` + `use std::path::PathBuf`) get merged into `use std::path::{Path, PathBuf}`.

Created `opencode-plugin-aft/src/tools/imports.ts` with Zod schemas for all 3 import tools following the established D034 pattern, and registered them in the plugin's tool registry alongside existing tool categories.

## Verification

- `cargo build` — 0 warnings ✅
- `cargo test` — 141 unit tests + 61 integration tests = 202 total, 0 failures ✅
- `cargo test --test integration -- remove_import` — 3 tests pass ✅
- `cargo test --test integration -- organize_imports` — 4 tests pass ✅
- `bun test` in `opencode-plugin-aft/` — 22 tests pass ✅
- S01 demo criterion verified: integration tests prove add_import places imports in correct group, alphabetized and deduplicated across all 6 languages

### Slice-level verification status (all checks):
- `cargo test` — all existing tests pass ✅
- `cargo test -- import` — import unit tests pass ✅
- `cargo test --test integration` — all 61 integration tests pass ✅
  - add_import correct group for all 6 languages ✅
  - add_import dedup ✅
  - add_import alphabetizes ✅
  - remove_import removes statement ✅
  - remove_import removes one name from multi-name import ✅
  - organize_imports re-sorts and re-groups ✅
- `bun test` — plugin tests pass ✅
- Error response tests: unsupported language → invalid_request, missing file → file_not_found, missing module → import_not_found ✅

## Diagnostics

- Send `remove_import` with module+optional name and check `removed`/`syntax_valid` in response
- Send `organize_imports` with file and check `groups` array (name + count per group) and `removed_duplicates` count
- Error responses include `code` field: `import_not_found`, `file_not_found`, `invalid_request`
- stderr shows `[aft] remove_import: {file}` and `[aft] organize_imports: {file}` on every call
- All mutations create backups inspectable via `edit_history`/`restore_checkpoint`

## Deviations

- Removed `is_merged` field from internal `OrganizedImport` struct — planned but unnecessary since the merge/non-merge distinction is fully captured by whether the module_path contains `{` braces. Cleaner without dead code.

## Known Issues

None.

## Files Created/Modified

- `src/commands/remove_import.rs` — new: remove_import handler (~210 lines)
- `src/commands/organize_imports.rs` — new: organize_imports handler (~460 lines including Rust use-tree merging)
- `src/commands/mod.rs` — added organize_imports and remove_import module declarations
- `src/main.rs` — added dispatch entries for remove_import and organize_imports
- `opencode-plugin-aft/src/tools/imports.ts` — new: plugin tool definitions for all 3 import commands (~90 lines)
- `opencode-plugin-aft/src/index.ts` — updated to import and register importTools
- `tests/integration/import_test.rs` — extended with 7 new tests: 3 for remove_import, 4 for organize_imports
- `.gsd/milestones/M002/slices/S01/tasks/T03-PLAN.md` — added Observability Impact section
