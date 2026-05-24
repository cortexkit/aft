---
id: T02
parent: S01
milestone: M002
provides:
  - Python import detection/parsing/generation (import X and from X import Y forms) with 3-group classification (stdlib/external/internal)
  - Rust use_declaration detection/parsing/generation with 3-group classification (std/external/internal)
  - Go import_declaration detection/parsing with 2-group classification (stdlib/external), handles both single and grouped forms
  - Unified ImportGroup enum (Stdlib/External/Internal) replacing the TS-only External/Relative
  - Whole-module dedup for Python/Rust/Go
  - Integration tests for add_import across all 3 new languages
key_files:
  - src/imports.rs
  - src/commands/add_import.rs
  - tests/fixtures/imports_py.py
  - tests/fixtures/imports_rs.rs
  - tests/fixtures/imports_go.go
  - tests/integration/import_test.rs
key_decisions:
  - "D053: ImportGroup refactored from 2-tier (External/Relative) to 3-tier (Stdlib/External/Internal) — all 6 languages map cleanly, Ord derive gives natural sort"
  - "D054: Whole-module dedup matches on module path alone when names and default_import are empty — needed for Python/Rust/Go where 'import X' has no named imports"
patterns_established:
  - "Per-language classify_group_*() functions with unified ImportGroup dispatch via classify_group(lang, path)"
  - "Python stdlib detection via embedded PYTHON_STDLIB const list — covers all Python 3.x stdlib modules"
  - "Go grouped import handling: parse individual specs from import_spec_list, generate tab-indented spec for insertion into existing group block"
  - "Rust pub use detection stored in default_import field as 'pub' marker"
observability_surfaces:
  - "add_import response 'group' field now returns 'stdlib'/'external'/'internal' (was 'external'/'relative' for TS only)"
  - "Dedup: already_present=true works for all 6 languages including module-level imports"
  - "stderr [aft] add_import: {file} fires for all language files — no new signal names needed"
duration: ~30min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Python, Rust, Go import support + add_import integration tests

**Extended the import engine to support Python, Rust, and Go with per-language parsing, group classification, and import generation — all proven by integration tests through the binary protocol.**

## What Happened

Extended `src/imports.rs` with three new language implementations:

**ImportGroup refactor**: Replaced the 2-group enum (External/Relative) with a 3-tier unified enum (Stdlib < External < Internal). All 6 languages map cleanly — TS/JS uses External+Internal, Python uses all three, Rust uses all three, Go uses Stdlib+External. Updated `find_insertion_point` to generalize across N groups using Ord-based neighbor lookup.

**Python**: Detects `import_statement` and `import_from_statement` nodes. Handles `import X`, `from X import Y, Z`, and relative `from . import Y` / `from ..mod import Z`. Embeds a comprehensive Python 3.x stdlib module list for group classification. Generates `import X` or `from X import Y` text.

**Rust**: Detects `use_declaration` nodes including `scoped_use_list` forms (e.g. `serde::{Deserialize, Serialize}`). Groups by first path segment: std/core/alloc → Stdlib, crate/self/super → Internal, else → External. Detects `pub use` via visibility_modifier. Generates `use path::to::Item;`.

**Go**: Detects `import_declaration` nodes, handling both single (`import "fmt"`) and grouped (`import (\n\t"path"\n)`) forms. Each `import_spec` parsed individually. Grouped import insertion generates tab-indented spec format; standalone insertion generates `import "path"`. Stdlib detection uses dot-in-path heuristic per goimports convention.

**Dedup fix**: `is_duplicate` was extended to handle "whole module" imports (empty names + no default import) by matching on module path alone — needed because Python/Rust/Go module imports aren't SideEffect kind.

**add_import handler**: Updated to use language-aware `classify_group()` dispatch and Go-specific grouped import detection.

## Verification

- `cargo test -- import` — 43 unit tests pass (22 T01 + 21 new)
- `cargo test --test integration` — 54 integration tests pass (19 import tests: 9 T01 + 10 new)
- `cargo test` — 141 unit + 54 integration = 195 total, 0 failures, 0 regressions
- Integration tests verify: group placement (Python stdlib/external/local, Rust std/external, Go stdlib/external), dedup for all 3 languages, existing TS/JS tests pass with new group labels

**Slice-level checks passing**: `cargo test`, `cargo test -- import`, `cargo test --test integration` all pass. Remaining slice checks (remove_import, organize_imports, plugin tests) are for later tasks.

## Diagnostics

- Send `add_import` with a module path and inspect `group` in response: "stdlib", "external", or "internal"
- `already_present: true` indicates dedup caught the import — works for all 6 languages
- stderr shows `[aft] add_import: {file}` for every call regardless of language
- Error responses: `code: "invalid_request"` for unsupported language, `code: "file_not_found"` for missing file

## Deviations

- **ImportGroup::Relative renamed to ImportGroup::Internal**: Not in the task plan but required for the unified 3-tier group enum. All TS/JS tests updated from checking "relative" to "internal". This is a semantic improvement — "internal" is more accurate across Python (local imports) and Rust (crate:: imports).
- **is_duplicate extended for whole-module imports**: Task plan didn't mention dedup needing changes, but without this fix Python/Rust/Go dedup tests fail because those languages use Value kind (not SideEffect) for plain module imports.

## Known Issues

- Rust `pub use` is stored in `default_import` field as a "pub" marker — works but is a semantic stretch. A dedicated `is_pub` flag would be cleaner.
- Python `from . import utils` parses the module_path as "." (just the dot). Works for classification but the raw module_path might surprise consumers expecting a dotted module name.
- Go `add_import` always generates import text but doesn't rewrite a single import into a grouped form — that's deferred to `organize_imports` (D045).

## Files Created/Modified

- `src/imports.rs` — Extended with Python/Rust/Go implementations (~300 lines), refactored ImportGroup enum, updated dedup logic
- `src/commands/add_import.rs` — Language-aware group classification and Go grouped import handling
- `tests/fixtures/imports_py.py` — Python import fixture with stdlib/third-party/local groups
- `tests/fixtures/imports_rs.rs` — Rust import fixture with std/external/crate groups
- `tests/fixtures/imports_go.go` — Go import fixture with single and grouped import forms
- `tests/integration/import_test.rs` — 10 new integration tests (Python stdlib/external/local/dedup, Rust std/external/dedup, Go stdlib/external/dedup)
- `.gsd/milestones/M002/slices/S01/tasks/T02-PLAN.md` — Added Observability Impact section
- `.gsd/DECISIONS.md` — Added D053 (ImportGroup 3-tier) and D054 (whole-module dedup)
