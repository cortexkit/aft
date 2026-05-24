---
estimated_steps: 5
estimated_files: 4
---

# T02: Python + Rust + Go symbol extraction and full test suite

**Slice:** S02 — Tree-sitter Multi-Language Engine
**Milestone:** M001

## Description

Complete the 6-language coverage by adding query patterns for Python, Rust, and Go. Each language has distinct AST challenges identified in research: Python uses `function_definition` and `class_definition` with indentation-based scope (parent walk needed for scope chains), Rust has `function_item` nodes inside `impl_item > declaration_list` for methods (scope chain must include type name + optional trait), Go uses `method_declaration` with `field_identifier` for receiver methods. The architecture is proven by T01 — this task extends it with 3 new query groups and fixture files.

## Steps

1. Write Python query pattern: capture `function_definition` (top-level and nested), `class_definition`, decorated functions/classes (`decorated_definition`). Build scope chain by walking parent nodes — `function_definition` inside `class_definition > block` gets scope chain `["ClassName"]`. Capture `@` decorator name for signature context.
2. Write Rust query pattern: capture `function_item` (free functions and methods), `struct_item`, `enum_item`, `trait_item`, `impl_item`. For methods: `function_item` inside `impl_item > declaration_list` — scope chain includes the impl target type. For trait impls (`impl Trait for Type`), scope chain should reflect both. Capture visibility modifiers (`pub`, `pub(crate)`) for export status.
3. Write Go query pattern: capture `function_declaration` (name via `identifier`), `method_declaration` (name via `field_identifier`, receiver from parameter list), `type_spec` inside `type_declaration` for struct and interface types. Go has no export keyword — export is uppercase first letter (detect via name inspection).
4. Create test fixture files: `tests/fixtures/sample.py` (functions, class with methods, decorated function, nested class), `tests/fixtures/sample.rs` (free function, struct, enum, trait, impl block with methods, trait impl with methods, pub items), `tests/fixtures/sample.go` (package, function, struct type, interface type, method with receiver, exported vs unexported names).
5. Write unit tests for Python, Rust, and Go: call `TreeSitterProvider::list_symbols()` on each fixture, assert correct symbol count, names, kinds, scope chains, signatures, export status. Add a cross-language test that verifies all 6 languages produce non-empty results from their fixtures.

## Must-Haves

- [ ] Python query detects: functions, classes, methods (with scope chain), decorated functions
- [ ] Rust query detects: free functions, struct, enum, trait, impl methods (scope chain includes type), trait impl methods, pub export detection
- [ ] Go query detects: functions, struct types, interface types, receiver methods (with scope chain), uppercase-name export detection
- [ ] Each language has a fixture file with representative code patterns
- [ ] Unit tests verify symbol count, names, kinds, scope chains for all 3 languages
- [ ] All 6 languages pass — cross-language test confirms no regressions in TS/JS/TSX

## Verification

- `cargo test` — all tests pass (S01 tests + T01 web language tests + T02 language tests)
- Python test: ≥4 symbols (function, class, method, decorated function), method has scope chain `["ClassName"]`
- Rust test: ≥6 symbols (function, struct, enum, trait, impl method, trait impl method), impl method scope chain includes type name
- Go test: ≥4 symbols (function, struct type, interface type, receiver method), receiver method has scope chain with type name, exported function detected via uppercase
- Cross-language: all 6 fixture files produce ≥2 symbols each

## Inputs

- `src/parser.rs` — FileParser and TreeSitterProvider from T01 (extend with new query patterns)
- `tests/fixtures/sample.ts`, `sample.tsx`, `sample.js` — existing fixtures from T01 (must not break)
- T01's query pattern approach — follow the same `include_str!()` and capture pattern conventions

## Observability Impact

- **New query compile failures**: If a Python/Rust/Go query pattern has syntax errors, `[aft] query compile failed for {lang}: {error}` is logged to stderr and `AftError::ParseError` returned with `code: "parse_error"`. Future agents see the exact query error in JSON response.
- **New language extraction failures**: If tree-sitter grammar returns `None` for a new language file, `[aft] parse failed for {path}` logged to stderr. Existing diagnostic pattern from T01 — no new log format.
- **Scope chain visibility**: Python methods get `scope_chain: ["ClassName"]`, Rust impl methods get `scope_chain: ["TypeName"]` or `scope_chain: ["Trait for TypeName"]`, Go receiver methods get `scope_chain: ["TypeName"]`. Agents can inspect scope chains to understand symbol nesting.
- **Export detection**: Rust symbols use `pub` visibility modifier. Go symbols use uppercase-first-letter convention. Both surface via `exported: true/false` in Symbol JSON — same field as TS/JS but different detection logic.

## Expected Output

- `src/parser.rs` — updated: Python, Rust, Go query patterns added
- `tests/fixtures/sample.py` — new: representative Python code
- `tests/fixtures/sample.rs` — new: representative Rust code (note: this is a test fixture, not the src file)
- `tests/fixtures/sample.go` — new: representative Go code
