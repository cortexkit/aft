# S02: Tree-sitter Multi-Language Engine — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: S02 is a library-level slice with no user-facing commands or runtime protocol surface. All symbol extraction is tested via unit tests against fixture files. The next slice (S03) exposes this through the JSON protocol — that's where live-runtime UAT applies.

## Preconditions

- Rust toolchain installed (rustc, cargo)
- Repository cloned at project root
- All dependencies fetched (`cargo fetch` or first build)

## Smoke Test

Run `cargo test --lib -- parser::tests::cross_language_all_six_produce_symbols` — should pass, confirming all 6 language grammars load and produce symbols.

## Test Cases

### 1. TypeScript symbol extraction — all kinds

1. Run `cargo test --lib -- parser::tests::ts_extracts_all_symbol_kinds`
2. Inspect `tests/fixtures/sample.ts` — must contain at least: a function declaration, an arrow function, a class with methods, an interface, an enum, and a type alias
3. **Expected:** Test passes. Extracted symbols include Function (greet, add), Class (UserService), Method (getUser, addUser), Interface (Config), Enum (Status), TypeAlias (UserId). At least 7 symbols total.

### 2. TypeScript export detection

1. Run `cargo test --lib -- parser::tests::ts_export_detection`
2. **Expected:** Test passes. `greet`, `add`, `UserService`, `Config`, `Status` have `exported: true`. `internalHelper` has `exported: false`.

### 3. TypeScript scope chains

1. Run `cargo test --lib -- parser::tests::ts_method_scope_chain`
2. **Expected:** Test passes. Methods `getUser` and `addUser` have `scope_chain: ["UserService"]` and `parent: Some("UserService")`.

### 4. Arrow function naming (TS/JS)

1. Run `cargo test --lib -- parser::tests::ts_extracts_all_symbol_kinds` and `parser::tests::js_arrow_fn_correctly_named`
2. **Expected:** Arrow functions assigned to `const` (e.g., `const add = () => ...`) are extracted as `SymbolKind::Function` with the variable name as the symbol name, not "anonymous".

### 5. TSX parsing — JSX doesn't break extraction

1. Run `cargo test --lib -- parser::tests::tsx_jsx_doesnt_break_parser`
2. **Expected:** Test passes. TSX file with JSX return values (`<div>`, `<button>`) parses without error. Symbols include Button (arrow function), Counter (class), formatLabel (function).

### 6. JavaScript class methods

1. Run `cargo test --lib -- parser::tests::js_method_scope_chain`
2. **Expected:** Test passes. Method inside `EventEmitter` class has scope chain `["EventEmitter"]`.

### 7. Python scope chains — class methods

1. Run `cargo test --lib -- parser::tests::py_method_scope_chain`
2. **Expected:** `__init__` and `instance_method` have `scope_chain: ["MyClass"]`. `inner_method` has `scope_chain: ["OuterClass", "InnerClass"]`.

### 8. Python decorated function signature

1. Run `cargo test --lib -- parser::tests::py_decorated_function_signature`
2. **Expected:** The decorated function's `signature` field includes the decorator text (e.g., contains `@staticmethod`).

### 9. Rust impl method scope chains

1. Run `cargo test --lib -- parser::tests::rs_impl_method_scope_chain`
2. **Expected:** Methods in `impl MyStruct` have `scope_chain: ["MyStruct"]`.

### 10. Rust trait impl scope chains

1. Run `cargo test --lib -- parser::tests::rs_trait_impl_scope_chain`
2. **Expected:** Methods in `impl Drawable for MyStruct` have `scope_chain: ["Drawable for MyStruct"]`.

### 11. Rust pub export detection

1. Run `cargo test --lib -- parser::tests::rs_pub_export_detection`
2. **Expected:** `public_function` has `exported: true`. `private_function` has `exported: false`.

### 12. Go receiver methods

1. Run `cargo test --lib -- parser::tests::go_receiver_method_scope_chain`
2. **Expected:** `String` method on `MyStruct` has `scope_chain: ["MyStruct"]`, `kind: Method`.

### 13. Go uppercase export detection

1. Run `cargo test --lib -- parser::tests::go_uppercase_export_detection`
2. **Expected:** `ExportedFunction` and `MyStruct` have `exported: true`. `unexportedFunction` and `helper` have `exported: false`.

### 14. Symbol signatures present

1. Run `cargo test --lib -- parser::tests::ts_signatures_present`
2. **Expected:** Every extracted symbol has a non-empty `signature` field containing recognizable declaration text.

### 15. Symbol ranges valid

1. Run `cargo test --lib -- parser::tests::ts_ranges_valid` and similar for py, rs, go
2. **Expected:** Every symbol has `range.start_line >= 1`, `range.end_line >= range.start_line`.

### 16. resolve_symbol — exact match and miss

1. Run `cargo test --lib -- parser::tests::resolve_symbol_finds_match` and `parser::tests::resolve_symbol_not_found`
2. **Expected:** Existing symbol name returns `Ok(SymbolMatch)`. Non-existent name returns `Err(AftError::SymbolNotFound)`.

### 17. Parse tree caching

1. Run `cargo test --lib -- parser::tests::parse_cache_returns_same_tree`
2. **Expected:** Second parse of the same file returns an identical tree root node, proving the cache was hit.

## Edge Cases

### Unsupported file extension

1. Run `cargo test --lib -- parser::tests::unsupported_extension_returns_invalid_request`
2. **Expected:** Parsing a file with extension `.xyz` returns `AftError::InvalidRequest` with message containing "unsupported file extension" and the extension name.

### JSX extension uses JavaScript grammar

1. Verify that `detect_language("file.jsx")` returns `Some(LangId::Js)` (not TSX or separate JSX grammar)
2. Run `cargo test --lib -- parser::tests::detect_jsx`
3. **Expected:** `.jsx` maps to the JavaScript grammar. JSX syntax in `.jsx` files parses correctly via the JS grammar.

### Cross-language minimum symbol count

1. Run `cargo test --lib -- parser::tests::cross_language_all_six_produce_symbols`
2. **Expected:** All 6 fixture files (sample.ts, sample.tsx, sample.js, sample.py, sample.rs, sample.go) produce at least 2 symbols each. No grammar loading failures.

## Failure Signals

- Any `cargo test` failure in `parser::tests` — indicates broken symbol extraction
- `cargo build` warnings about tree-sitter or grammar crates — indicates version compatibility issues
- Missing or empty fixture files — extraction tests would pass vacuously
- `QueryError` at runtime — indicates malformed .scm query pattern
- `RefCell` borrow panic — indicates concurrent access to `TreeSitterProvider` (shouldn't happen in single-threaded binary)

## Requirements Proved By This UAT

- R002 — Multi-language tree-sitter parsing: all 6 languages parse correctly, symbols extracted with names, kinds, ranges, signatures, scope chains, export status
- R034 — Web-first language priority: TS/JS/TSX tested first (T01), then Python/Rust/Go (T02)

## Not Proven By This UAT

- Symbol extraction is not tested through the JSON protocol (outline/zoom commands) — that's S03
- No runtime persistence testing — parse tree cache is only verified within a single test process
- No cross-file symbol resolution — S02 is single-file only
- No edit-triggered re-parse verification — that's S05

## Notes for Tester

- All test cases map directly to existing automated tests. Run `cargo test` to execute all 57 at once.
- If adding new test fixtures, ensure they exercise the specific language constructs listed in each test case — the tests assert on symbol counts and specific names.
- Python `exported` is always `false` — this is correct behavior, not a bug. Python has no syntactic export marker.
- `TypeAlias` is a 7th SymbolKind not in the original plan — downstream slices should handle it in exhaustive matches.
