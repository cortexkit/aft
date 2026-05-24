---
estimated_steps: 6
estimated_files: 10
---

# T02: Compound operations — `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`

**Slice:** S02 — Scope-aware Insertion & Compound Operations
**Milestone:** M002

## Description

Four language-specific structural transforms that agents do most clumsily. Each follows the standard command handler pattern but uses distinct AST navigation. `add_derive` (Rust) manipulates attribute items as siblings of struct/enum nodes. `wrap_try_catch` (TS/JS) re-indents function bodies and wraps in try/catch. `add_decorator` (Python) inserts decorator lines handling already-decorated functions. `add_struct_tags` (Go) parses and manipulates backtick-delimited struct tag strings. This is the R015 deliverable.

## Steps

1. **Build `src/commands/add_derive.rs`** — Params: `file`, `target` (struct/enum name), `derives` (array of derive names to add). Parse AST, find `struct_item` or `enum_item` by name. Walk backward through preceding siblings to find `attribute_item` nodes containing `derive`. If found: parse the `token_tree` text to extract existing derive names, append new ones (dedup), regenerate the attribute text. If not found: insert a new `#[derive(...)]` line before the target. Auto-backup, write, validate. Tests: append to existing derive, create new derive, dedup existing derive name.

2. **Build `src/commands/wrap_try_catch.rs`** — Params: `file`, `target` (function/method name), `catch_body` (optional, defaults to `throw error;`). Parse AST, find `function_declaration` or `method_definition` by name. Extract `statement_block` body content (between `{` and `}`). Re-indent each body line +1 level using `indent.rs`. Build wrapped body: `{\n  try {\n    ...body...\n  } catch (error) {\n    ...catch_body...\n  }\n}`. Replace the original statement_block range. Limit to functions with `statement_block` bodies — return error for arrow functions without braces. Auto-backup, write, validate. Tests: wrap simple function, wrap method in class, verify indentation preserved.

3. **Build `src/commands/add_decorator.rs`** — Params: `file`, `target` (function/class name), `decorator` (decorator text without `@`, e.g. `"cache"` or `"app.route('/users')"` ), `position` (optional: `first` or `last` among existing decorators, default `first`). Parse AST, find `function_definition` or `class_definition` by name. Check if the target has a `decorated_definition` parent — if so, insert among existing decorators. If not, insert `@decorator` line before the def with matching indentation. Auto-backup, write, validate. Tests: add decorator to plain function, add decorator to already-decorated function (both positions), verify indentation.

4. **Build `src/commands/add_struct_tags.rs`** — Params: `file`, `target` (struct name), `field` (field name), `tag` (tag key, e.g. `"json"`), `value` (tag value, e.g. `"user_name,omitempty"`). Parse AST, find `type_declaration` → `struct_type`, find `field_declaration` matching field name. If field already has a `raw_string_literal` tag child: parse existing tag string, add/update the key-value pair, regenerate. If no tag: append `` `tag:"value"` `` after the type. Auto-backup, write, validate. Tests: add tag to field without existing tags, add tag to field with existing tags, update existing tag value.

5. **Wire all 4 commands into dispatch** — Add module declarations to `src/commands/mod.rs`. Add dispatch arms to `src/main.rs`.

6. **Write integration tests** — `tests/integration/structure_test.rs` with `temp_copy` pattern. Create fixture files: `tests/fixtures/structure_rs.rs` (struct with derive), `tests/fixtures/structure_ts.ts` (function to wrap), `tests/fixtures/structure_py.py` (functions with/without decorators), `tests/fixtures/structure_go.go` (struct with fields). Register module in `tests/integration/main.rs`.

## Must-Haves

- [ ] `add_derive` appends to existing `#[derive(...)]` attribute without duplicating
- [ ] `add_derive` creates new `#[derive(...)]` when no derive exists
- [ ] `wrap_try_catch` preserves body indentation inside the try block
- [ ] `wrap_try_catch` errors on arrow functions without statement_block bodies
- [ ] `add_decorator` handles both plain and already-decorated functions
- [ ] `add_decorator` preserves indentation of the function/class definition
- [ ] `add_struct_tags` adds to fields without existing tags
- [ ] `add_struct_tags` updates existing tags without losing other tag keys
- [ ] All 4 commands auto-backup and validate syntax

## Verification

- `cargo build 2>&1 | grep -c warning` → 0
- `cargo test -- structure` — all compound operation integration tests pass
- `cargo test -- add_derive` — derive-specific tests pass
- `cargo test` — no regressions

## Inputs

- `src/indent.rs` — indentation detection and generation from T01
- `src/parser.rs` — FileParser, detect_language, grammar_for, node_text, node_range (pub(crate) from T01)
- `src/edit.rs` — replace_byte_range, auto_backup, validate_syntax
- S02-RESEARCH.md — AST node type reference for compound operation targets

## Expected Output

- `src/commands/add_derive.rs` — Rust derive manipulation handler (~120 lines)
- `src/commands/wrap_try_catch.rs` — TS/JS try-catch wrapping handler (~150 lines)
- `src/commands/add_decorator.rs` — Python decorator insertion handler (~120 lines)
- `src/commands/add_struct_tags.rs` — Go struct tag manipulation handler (~140 lines)
- `tests/integration/structure_test.rs` — integration tests (~300 lines)
- `tests/fixtures/structure_*.{rs,ts,py,go}` — 4 fixture files

## Observability Impact

- **Runtime signals:** Each command logs `[aft] add_derive: {file}` / `[aft] wrap_try_catch: {file}` / `[aft] add_decorator: {file}` / `[aft] add_struct_tags: {file}` on stderr per invocation.
- **Error responses:** Structured `code` field — `scope_not_found` (with available scopes), `field_not_found` (with field name), `target_not_found` (with target name and available targets), `invalid_request` (with supported params).
- **Inspection surface:** Success responses include `file`, `target`, `syntax_valid`, and `backup_id`. `add_derive` additionally returns the final derive list. `add_struct_tags` returns the final tag string.
- **Failure visibility:** Target not found errors include available target names in the scope. Field not found errors include the searched field name and struct name.
