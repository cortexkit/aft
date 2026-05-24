---
estimated_steps: 5
estimated_files: 6
---

# T02: Python, Rust, Go import support + add_import integration tests

**Slice:** S01 — Import Management
**Milestone:** M002

## Description

Extend the import engine from T01 to support Python, Rust, and Go. Each language has distinct import conventions and tree-sitter node types. The shared types and `add_import` command handler are already proven — this task adds the per-language implementations and integration tests.

- **Python**: `import_statement` and `import_from_statement` nodes. 3 groups: stdlib (embedded module list), third-party, local (relative with `.`/`..`). isort convention.
- **Rust**: `use_declaration` nodes. 3 groups: std/core/alloc, external crates, crate::/self::/super::. `add_import` creates new `use` lines (D045 — merging deferred to `organize_imports`).
- **Go**: `import_declaration` containing `import_spec` nodes. 2 groups: stdlib (no dots in import path), external (dots in path). goimports convention.

## Steps

1. Add Python import implementation to `src/imports.rs`:
   - Detect `import_statement` and `import_from_statement` as direct children of `module` (root node)
   - Parse: extract module name, imported names (for `from X import a, b`), relative level (`.`, `..`)
   - Group classification: relative (starts with `.`) → Local, check against embedded stdlib list → Stdlib, else → ThirdParty
   - Embed a Python 3.x stdlib module list (known finite set: `os`, `sys`, `json`, `pathlib`, `typing`, `collections`, etc.)
   - Text generation: `import module`, `from module import name1, name2`, `from . import name`

2. Add Rust import implementation to `src/imports.rs`:
   - Detect `use_declaration` as direct children of `source_file` (root node)
   - Parse: extract the full path from `use_path`/`scoped_identifier` children
   - Group classification: starts with `std::`/`core::`/`alloc::` → Std, starts with `crate::`/`self::`/`super::` → Internal, else → External
   - Text generation: `use path::to::Item;`, handle `pub use` when the original has visibility modifier

3. Add Go import implementation to `src/imports.rs`:
   - Detect `import_declaration` nodes at top level. Handle both single (`import "fmt"`) and grouped (`import (\n"fmt"\n"os"\n)`) forms
   - Parse: extract import path string, optional alias
   - Group classification: path contains `.` → External, else → Stdlib
   - For grouped imports: insert new spec into the correct group within the existing `import()` block. For single imports: may need to convert to grouped form or add a new single import
   - Text generation: `import "path"`, handle alias `import alias "path"`

4. Create language-specific import fixtures:
   - `tests/fixtures/imports_py.py` — Python file with stdlib, third-party, and local imports in isort-style groups
   - `tests/fixtures/imports_rs.rs` — Rust file with std, external crate, and crate-internal use declarations
   - `tests/fixtures/imports_go.go` — Go file with stdlib and external import groups (both single and grouped forms)

5. Add integration tests to `tests/integration/import_test.rs`:
   - `add_import` places a stdlib import into the stdlib group (Python)
   - `add_import` places a third-party import into the third-party group (Python)
   - `add_import` places a relative import into the local group (Python)
   - `add_import` creates a new `use` declaration in the correct group (Rust)
   - `add_import` places a stdlib import into the stdlib group (Go)
   - `add_import` places an external import into the external group (Go)
   - Dedup works for all 3 languages

## Must-Haves

- [ ] Python import detection and parsing (both `import X` and `from X import Y` forms)
- [ ] Python 3-group classification with embedded stdlib module list
- [ ] Rust `use_declaration` detection and parsing with 3-group classification
- [ ] Go `import_declaration` detection and parsing with 2-group classification
- [ ] Go handles both single-import and grouped-import forms
- [ ] Integration tests proving `add_import` for all 3 languages with correct group placement

## Verification

- `cargo test -- import` — all import tests pass (T01 tests still pass + new language tests)
- `cargo test --test integration` — full integration suite passes
- `cargo test` — 0 regressions

## Observability Impact

- **Runtime signals**: `[aft] add_import: {file}` on stderr now fires for Python, Rust, and Go files (same pattern as TS/JS, no new signal names — the existing log line covers all languages)
- **Inspection**: Send `add_import` with a module path and check `group` field in response: "stdlib" (Python/Rust/Go), "external" (all languages), "internal" (Python/Rust relative, TS/JS relative)
- **Dedup inspection**: `already_present: true` in response indicates the import already exists — works for all 6 languages now
- **Error responses**: `code: "invalid_request"` for unsupported language, `code: "file_not_found"` for missing file — unchanged from T01
- **Failure visibility**: If a language's tree-sitter parse fails, the error propagates through the existing `ParseError` path with a descriptive message

## Inputs

- `src/imports.rs` — T01's import engine with shared types and TS/JS/TSX implementation
- `src/commands/add_import.rs` — T01's command handler (no changes needed — it dispatches on LangId)
- `src/parser.rs` — tree-sitter grammars for Python, Rust, Go already embedded
- T01's integration test patterns as reference

## Expected Output

- `src/imports.rs` — extended with Python, Rust, Go implementations (~200-300 additional lines)
- `tests/fixtures/imports_py.py` — Python import fixture
- `tests/fixtures/imports_rs.rs` — Rust import fixture
- `tests/fixtures/imports_go.go` — Go import fixture
- `tests/integration/import_test.rs` — extended with Python/Rust/Go tests (~100-150 additional lines)
