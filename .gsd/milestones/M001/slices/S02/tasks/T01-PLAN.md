---
estimated_steps: 8
estimated_files: 9
---

# T01: Parser infrastructure + TS/JS/TSX symbol extraction

**Slice:** S02 — Tree-sitter Multi-Language Engine
**Milestone:** M001

## Description

Establish the tree-sitter parsing foundation and deliver symbol extraction for TypeScript, JavaScript, and TSX — the three highest-value languages (D004). This creates the `FileParser` (language detection, parse tree caching), the expanded `Symbol` type system (`SymbolKind` enum, signature, scope chain, export status), and the `TreeSitterProvider` that replaces the S01 `StubProvider`. Query patterns for TS (shared with TSX) and JS must handle the pitfalls identified in research: arrow functions assigned to const, `type_identifier` vs `identifier` for class/interface names, and `export_statement` wrapper detection.

## Steps

1. Add tree-sitter dependencies to `Cargo.toml`: `tree-sitter` 0.24, `tree-sitter-typescript`, `tree-sitter-javascript`, `tree-sitter-python`, `tree-sitter-rust`, `tree-sitter-go`. Run `cargo build` to verify grammar crate compatibility.
2. Create `src/symbols.rs`: `SymbolKind` enum (Function, Class, Method, Struct, Interface, Enum), expanded `Symbol` struct (name, kind, range, signature, scope_chain as `Vec<String>`, exported as bool, parent as `Option<String>`), `Range` struct with Serialize. Remove the old Symbol/Range from `language.rs` — import from `symbols.rs` instead.
3. Create `src/parser.rs`: `FileParser` struct with language detection (extension → grammar mapping for all 6 extensions: .ts, .tsx, .js, .jsx, .py, .rs, .go), parse tree caching (`HashMap<PathBuf, (SystemTime, Tree)>` with mtime invalidation), and query pattern loading via `include_str!()`. `TreeSitterProvider` struct wrapping `FileParser`, implementing `LanguageProvider` trait.
4. Write TypeScript query pattern (also used for TSX): capture function_declaration, arrow_function assigned to const/let/var, class_declaration with method_definition, interface_declaration, enum_declaration, type_alias_declaration. Capture export_statement wrappers for export detection. Use `type_identifier` for class/interface names, `identifier` for functions.
5. Write JavaScript query pattern: same as TS minus interface_declaration, enum_declaration, type_alias_declaration. Arrow function detection is critical — same `lexical_declaration > variable_declarator > arrow_function` pattern.
6. Create test fixture files: `tests/fixtures/sample.ts` (function, arrow fn, class with methods, interface, enum, exports, type alias), `tests/fixtures/sample.tsx` (React component as arrow fn + regular TS constructs), `tests/fixtures/sample.js` (function, arrow fn, class with methods, exports).
7. Update `src/language.rs`: keep `LanguageProvider` trait, `SymbolMatch`, `StubProvider` but switch Symbol and Range to re-exports from `symbols.rs`. Update `src/lib.rs` to declare `symbols` and `parser` modules.
8. Wire `TreeSitterProvider` into `src/main.rs` — instantiate it instead of (or alongside) any reference to `StubProvider`. Write unit tests in `parser.rs` (or `lib.rs` test module) that call `list_symbols()` on each TS/JS/TSX fixture file and assert on symbol names, kinds, ranges, signatures, scope chains, and export status.

## Must-Haves

- [ ] All 5 grammar crates compile together without version conflicts
- [ ] SymbolKind enum with Function, Class, Method, Struct, Interface, Enum variants
- [ ] Symbol struct with name, kind, range, signature, scope_chain, exported, parent — all Serialize
- [ ] FileParser detects language from .ts/.tsx/.js/.jsx/.py/.rs/.go extensions
- [ ] Parse tree caching with mtime invalidation
- [ ] TS query detects: function declarations, arrow functions assigned to const, class methods, interfaces, enums, exported items
- [ ] JS query detects: function declarations, arrow functions assigned to const, class methods, exported items
- [ ] TSX files parsed with TSX grammar variant, TS query patterns reused
- [ ] TreeSitterProvider implements LanguageProvider::list_symbols and resolve_symbol
- [ ] Unit tests prove correct extraction for each of the 3 web languages

## Verification

- `cargo build` — 0 errors, 0 warnings (grammar crate compatibility proven)
- `cargo test` — all existing S01 tests still pass + new symbol extraction tests pass
- TS test: ≥6 symbols extracted (function, arrow fn, class, method, interface, enum), correct names and kinds
- JS test: ≥4 symbols extracted (function, arrow fn, class, method), arrow fn correctly named
- TSX test: ≥2 symbols extracted (React component, at least one other), TSX grammar doesn't choke on JSX

## Observability Impact

- FileParser logs parse failures to stderr with `[aft]` prefix (consistent with S01 pattern)
- AftError::ParseError used for tree-sitter parse failures — structured error visible in JSON responses

## Inputs

- `src/language.rs` — LanguageProvider trait, Symbol, Range, SymbolMatch types (to be evolved)
- `src/error.rs` — AftError::ParseError, SymbolNotFound, AmbiguousSymbol variants (used as-is)
- `src/main.rs` — command dispatch loop (wire TreeSitterProvider into it)
- `src/lib.rs` — module declarations (add new modules)
- S01 summary — forward intelligence on params handling, match dispatch pattern

## Expected Output

- `Cargo.toml` — updated with tree-sitter + 5 grammar crate dependencies
- `src/symbols.rs` — new: SymbolKind, Symbol, Range with Serialize
- `src/parser.rs` — new: FileParser, TreeSitterProvider with TS/JS/TSX query patterns
- `src/language.rs` — evolved: uses Symbol/Range from symbols.rs
- `src/lib.rs` — updated: declares symbols, parser modules
- `src/main.rs` — updated: instantiates TreeSitterProvider
- `tests/fixtures/sample.ts` — new: representative TS code with all symbol kinds
- `tests/fixtures/sample.tsx` — new: React component + TS constructs
- `tests/fixtures/sample.js` — new: representative JS code with arrow functions
