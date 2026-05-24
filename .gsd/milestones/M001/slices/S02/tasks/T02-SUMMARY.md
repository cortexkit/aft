---
id: T02
parent: S02
milestone: M001
provides:
  - Python symbol extraction (functions, classes, methods, decorated functions with scope chains)
  - Rust symbol extraction (functions, structs, enums, traits as Interface, impl methods with scope chains, trait impl methods, pub export detection)
  - Go symbol extraction (functions, struct/interface types, receiver methods with scope chains, uppercase-first-letter export detection)
  - Cross-language test confirming all 6 languages produce ≥2 symbols each
key_files:
  - src/parser.rs
  - tests/fixtures/sample.py
  - tests/fixtures/sample.rs
  - tests/fixtures/sample.go
key_decisions:
  - Rust traits mapped to SymbolKind::Interface — closest semantic match, consistent with LSP symbol categorization
  - Rust trait impl methods get scope_chain ["Trait for Type"] while inherent impl methods get ["Type"] — distinguishes the two impl contexts
  - Python scope chains built by parent-node walking rather than query captures — tree-sitter queries can't express arbitrary ancestor relationships
  - Go export detection via first-character uppercase check — standard Go convention, no AST support needed
  - Decorated Python functions: signature includes decorator text (e.g. "@staticmethod\ndef foo():")
patterns_established:
  - Per-language extract function pattern extended: extract_py_symbols, extract_rs_symbols, extract_go_symbols — consistent with extract_ts_symbols and extract_js_symbols from T01
  - Rust impl method extraction walks impl_item children directly rather than using query captures — tree-sitter queries can't relate a function_item to its grandparent impl_item type
  - Go receiver type extracted via recursive type_identifier search in first parameter_list
observability_surfaces:
  - Query compile failures for new languages logged to stderr with [aft] prefix
  - Parse failures for .py/.rs/.go files use same AftError::ParseError path as TS/JS
  - Scope chain visibility in Symbol JSON output — agents can inspect method nesting
  - Export detection in Symbol.exported field for all 6 languages
duration: 1 task context
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Python + Rust + Go symbol extraction and full test suite

**Complete 6-language tree-sitter coverage — Python, Rust, Go query patterns + extraction functions, 3 fixture files, 17 new unit tests, cross-language verification.**

## What Happened

Added three query pattern constants (PY_QUERY, RS_QUERY, GO_QUERY) and three extraction functions to `src/parser.rs`. Each language has distinct challenges handled:

- **Python**: Query captures `function_definition`, `class_definition`, and `decorated_definition`. Scope chains built by walking parent nodes — a function inside `class_definition > block` gets `["ClassName"]` in its scope chain. Nested classes produce multi-level scope chains (e.g. `["OuterClass", "InnerClass"]`). Decorated functions include decorator text in their signature.

- **Rust**: Query captures `function_item`, `struct_item`, `enum_item`, `trait_item`, and `impl_item`. Free functions vs impl methods distinguished by checking if parent node is `declaration_list`. Impl block type names extracted from child `type_identifier` nodes — inherent impl gives scope `["MyStruct"]`, trait impl gives `["Drawable for MyStruct"]`. Visibility (`pub`) detected by checking for `visibility_modifier` child node.

- **Go**: Query captures `function_declaration`, `method_declaration`, and `type_spec` inside `type_declaration`. Receiver type extracted by finding `type_identifier` in the first `parameter_list` of `method_declaration`. Struct vs interface distinguished by `type_body` node kind. Export detection via uppercase first character.

Updated `query_for()` to return patterns for all 6 languages (was returning `None` for Python/Rust/Go). Updated `extract_symbols()` match arm to dispatch to the new extraction functions (was returning `Ok(vec![])` for the three languages).

## Verification

- `cargo build` — 0 warnings
- `cargo test` — 53 unit tests + 4 integration tests = 57 total, all green
- **Python** (9 symbols): top_level_function, MyClass, __init__, instance_method, decorated_function, OuterClass, InnerClass, inner_method, outer_method. Method scope chain `["MyClass"]` verified. Nested scope `["OuterClass", "InnerClass"]` verified. Decorator in signature verified.
- **Rust** (9 symbols): public_function, private_function, MyStruct, Color, Drawable, new, helper, draw, area. Impl method scope `["MyStruct"]` verified. Trait impl scope `["Drawable for MyStruct"]` verified. Pub export detection verified.
- **Go** (6 symbols): ExportedFunction, unexportedFunction, MyStruct, Reader, String, helper. Receiver method scope `["MyStruct"]` verified. Uppercase export detection verified.
- **Cross-language**: all 6 fixture files produce ≥2 symbols each — confirmed.
- All T01 tests (TS/JS/TSX) continue to pass — no regressions.
- Slice-level verification: all checks pass (this is the final task of S02).

## Diagnostics

- Parse/query failures for Python/Rust/Go files surface via same `[aft]` stderr logging and `AftError::ParseError` JSON response as TS/JS — no new diagnostic format.
- Scope chains visible in `Symbol.scope_chain` JSON field — agents inspect method nesting context.
- Export status visible in `Symbol.exported` field — different detection logic per language (TS/JS: export statement, Rust: pub modifier, Go: uppercase name, Python: always false).

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/parser.rs` — added PY_QUERY, RS_QUERY, GO_QUERY constants; extract_py_symbols, extract_rs_symbols, extract_go_symbols functions; py_scope_chain helper; extract_go_receiver_type + find_type_identifier_recursive helpers; 17 new unit tests + 1 cross-language test
- `tests/fixtures/sample.py` — new: Python fixture with functions, class with methods, decorated function, nested class
- `tests/fixtures/sample.rs` — new: Rust fixture with free functions, struct, enum, trait, impl methods, trait impl methods, pub/private items
- `tests/fixtures/sample.go` — new: Go fixture with exported/unexported functions, struct/interface types, receiver methods
- `.gsd/milestones/M001/slices/S02/tasks/T02-PLAN.md` — added Observability Impact section (pre-flight fix)
