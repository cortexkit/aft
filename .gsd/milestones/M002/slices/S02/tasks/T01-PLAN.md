---
estimated_steps: 7
estimated_files: 8
---

# T01: Indentation utility and `add_member` command

**Slice:** S02 — Scope-aware Insertion & Compound Operations
**Milestone:** M002

## Description

Build the shared indentation detection utility (`src/indent.rs`) and the `add_member` command handler that inserts methods, fields, or functions into scope containers (classes, structs, impl blocks) with correct indentation across all 4 language families (TS/JS, Python, Rust, Go). This is the core R014 deliverable and the most technically challenging part of S02 — Python uses indentation as scope, each language has different AST node structures for scope containers, and position resolution (`first`, `last`, `before:name`, `after:name`) requires walking named children.

## Steps

1. **Build `src/indent.rs`** — `detect_indent(source: &str) -> IndentStyle` analyzes indented lines to determine tabs vs spaces and width. `IndentStyle` enum with `Tabs`, `Spaces(u8)`. `indent_string(style: &IndentStyle) -> &str` returns the whitespace string. Language-specific defaults: Python 4 spaces, TS/JS 2 spaces, Rust 4 spaces, Go tabs. Confidence threshold: use detected style if >50% of indented lines agree, else fall back to language default. Add `pub mod indent` to `src/lib.rs`.

2. **Make `node_text()` and `node_range()` pub(crate) in `src/parser.rs`** — These are currently private helper functions. `add_member` needs them for scope container detection and member name extraction. Simple visibility change.

3. **Build `src/commands/add_member.rs`** — Handler following the command pattern (D026). Params: `file` (required), `scope` (required — name of class/struct/impl to target), `code` (required — the member code to insert), `position` (optional — `first`, `last`, `before:name`, `after:name`, default `last`). Flow: extract params → validate file exists + language supported → parse AST → find scope container node by name → find body node → detect indentation from existing children (or use file-level default for empty bodies) → resolve insertion byte offset from position → indent provided code lines to match → auto_backup → replace_byte_range(source, offset, offset, indented_code) → write → validate_syntax → respond. Response: `{ file, scope, position, syntax_valid?, backup_id? }`.

4. **Implement scope container finding per language** — TS/JS: walk root children for `class_declaration` matching name in `type_identifier` child, body is `class_body`. Python: walk for `class_definition` matching `identifier` child, body is `block`. Rust: walk for `impl_item` matching type name in children (handle both inherent `impl Foo` and trait `impl Trait for Foo`), body is `declaration_list`. Also walk for `struct_item` matching `type_identifier`, body is `field_declaration_list`. Go: walk for `type_declaration` → `type_spec` → `struct_type`, body is `field_declaration_list`. When scope name is ambiguous, return disambiguation response with candidates (D032).

5. **Implement position resolution** — `first`: insert after opening `{` or after `:` (Python) with newline. `last`: insert before closing `}` or at end of last child (Python). `before:name` / `after:name`: walk body's named children to find matching member name, error with `member_not_found` if not found. Empty containers: insert between delimiters with one level of indent.

6. **Create test fixtures** — `tests/fixtures/member_ts.ts` (class with methods), `tests/fixtures/member_py.py` (class with methods at 4-space indent), `tests/fixtures/member_rs.rs` (struct + impl block with methods), `tests/fixtures/member_go.go` (struct with fields).

7. **Write integration tests** — `tests/integration/member_test.rs` with `temp_copy` pattern from import tests. Tests: add method to TS class (last position), add method to Python class (verify indentation matches), add field to Rust struct, add method to Rust impl block, add field to Go struct, position `first` and `after:name`, empty class insertion, scope not found error. Register module in `tests/integration/main.rs`.

## Must-Haves

- [ ] `detect_indent()` correctly identifies tabs, 2-space, and 4-space indentation from source content
- [ ] `add_member` finds scope containers for TS/JS classes, Python classes, Rust structs/impl blocks, Go structs
- [ ] Inserted code matches existing indentation in all 4 language families
- [ ] Position modes `first`, `last`, `before:name`, `after:name` all work
- [ ] Empty scope container insertion works (class with no existing members)
- [ ] Scope-not-found returns error with `scope_not_found` code
- [ ] `before:name`/`after:name` with missing member returns `member_not_found` error
- [ ] Auto-backup and syntax validation on every mutation

## Verification

- `cargo build 2>&1 | grep -c warning` → 0
- `cargo test -- indent` — unit tests for indentation detection pass
- `cargo test -- member` — all add_member integration tests pass
- `cargo test` — no regressions in existing tests

## Observability Impact

- **New stderr log:** `[aft] add_member: {file}` emitted on every successful `add_member` call — future agents grep stderr for this to confirm the command ran.
- **Structured error responses:** `scope_not_found` (includes the scope name searched and list of available scopes), `member_not_found` (includes the member name searched within the scope) — future agents parse `code` + `message` fields to decide retry strategy.
- **Failure visibility:** When scope resolution fails, the error message lists available scope containers in the file, enabling the agent to correct the `scope` param without re-parsing.

## Inputs

- `src/edit.rs` — `replace_byte_range`, `auto_backup`, `validate_syntax`
- `src/parser.rs` — `FileParser`, `detect_language`, `grammar_for`, `LangId`, `node_text`, `node_range`
- `src/commands/add_import.rs` — handler pattern reference
- S02-RESEARCH.md — AST node type reference for scope containers per language

## Expected Output

- `src/indent.rs` — shared indentation detection utility (~60-80 lines)
- `src/commands/add_member.rs` — scope-aware member insertion handler (~300 lines)
- `src/parser.rs` — `node_text`, `node_range` changed to `pub(crate)`
- `tests/integration/member_test.rs` — integration tests (~200 lines)
- `tests/fixtures/member_*.{ts,py,rs,go}` — 4 fixture files
