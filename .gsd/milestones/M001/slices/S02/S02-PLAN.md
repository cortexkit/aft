# S02: Tree-sitter Multi-Language Engine

**Goal:** Binary parses files in all 6 languages (TS, JS, TSX, Python, Rust, Go) and extracts symbols — functions, classes, methods, structs, interfaces, enums — with names, kinds, ranges, signatures, scope chains, and export status via tree-sitter.

**Demo:** Unit tests prove correct symbol extraction from representative code files in all 6 languages, covering key constructs: arrow functions (TS/JS), impl block methods (Rust), receiver methods (Go), decorated class methods (Python), and export detection (TS/JS).

## Must-Haves

- `SymbolKind` enum covering function, class, method, struct, interface, enum
- `Symbol` struct with name, kind, range, signature, scope_chain, exported, parent — `Serialize` for downstream JSON responses
- `FileParser` with file extension → tree-sitter grammar mapping for all 6 languages + parse tree caching (mtime invalidation)
- `TreeSitterProvider` implementing `LanguageProvider` trait from S01
- Per-language query patterns: `typescript.scm` (shared with TSX), `javascript.scm`, `python.scm`, `rust.scm`, `go.scm`
- Arrow function detection: `const foo = () => ...` identified as function named `foo` (TS/JS)
- Rust methods inside `impl` blocks detected with correct scope chain (`TypeName.method_name`)
- Go receiver methods detected via `method_declaration` with `field_identifier`
- Python class methods detected with scope chain (`ClassName.method_name`)
- Export detection for TS/JS (`export` and `export default` wrappers)
- Representative test fixture files per language exercising all symbol kinds

## Proof Level

- This slice proves: contract (TreeSitterProvider correctly implements LanguageProvider for all 6 languages)
- Real runtime required: no (library-level unit tests against fixture files)
- Human/UAT required: no

## Verification

- `cargo build` — 0 warnings, all 5 grammar crates + tree-sitter compile without version conflicts
- `cargo test` — all tests pass, including per-language symbol extraction tests
- Each language has tests verifying: correct symbol count, symbol names, symbol kinds, range accuracy, signature extraction, scope chain for nested symbols, export detection (TS/JS)
- Test fixture files in `tests/fixtures/`: `sample.ts`, `sample.tsx`, `sample.js`, `sample.py`, `sample.rs`, `sample.go`
- Failure-path test: parsing a file with an unsupported extension returns `AftError::InvalidRequest` with `code: "invalid_request"` — verifies structured error output for bad inputs

## Observability / Diagnostics

- `FileParser` logs parse failures to stderr with `[aft]` prefix, including the file path and error detail — consistent with S01 log pattern
- `AftError::ParseError` used for tree-sitter parse failures — surfaces in JSON error responses with `code: "parse_error"` and descriptive message
- Unsupported file extensions return `AftError::InvalidRequest` with `code: "invalid_request"` listing the extension — agents see exactly why a file was rejected
- Parse tree cache hits/misses are silent by default (no log spam), but cache miss triggers a re-parse with error handling

## Integration Closure

- Upstream surfaces consumed: `src/language.rs` (LanguageProvider trait, Symbol/Range types), `src/error.rs` (AftError::ParseError, SymbolNotFound, AmbiguousSymbol)
- New wiring introduced: TreeSitterProvider instantiated in `main.rs`, replaces StubProvider; new modules `symbols`, `parser` added to `lib.rs`
- What remains before the milestone is truly usable end-to-end: S03 adds outline/zoom commands that expose symbol extraction through the JSON protocol

## Tasks

- [x] **T01: Parser infrastructure + TS/JS/TSX symbol extraction** `est:2h`
  - Why: Establishes the tree-sitter parsing architecture and delivers the 3 highest-value web languages first (D004). TS/JS/TSX share ~80% of query patterns and are where most AI agents operate. This is the riskiest work — grammar crate version compatibility and query pattern accuracy must be proven here.
  - Files: `Cargo.toml`, `src/symbols.rs`, `src/parser.rs`, `src/language.rs`, `src/lib.rs`, `src/main.rs`, `tests/fixtures/sample.ts`, `tests/fixtures/sample.tsx`, `tests/fixtures/sample.js`
  - Do: Add tree-sitter + 5 grammar crate deps. Create `symbols.rs` with `SymbolKind` enum and expanded `Symbol` struct. Create `parser.rs` with `FileParser` (language detection for all 6 extensions, parse tree caching, query execution) and `TreeSitterProvider`. Write TS query patterns (shared for TSX), JS query patterns. Must handle arrow functions assigned to const, class methods, interfaces, enums, export wrappers. Evolve `language.rs` types to use new Symbol. Wire TreeSitterProvider into main.rs. Write unit tests against TS/JS/TSX fixture files.
  - Verify: `cargo build` — 0 warnings; `cargo test` — TS/JS/TSX extraction tests pass with correct symbol names, kinds, ranges, signatures, scope chains, export status
  - Done when: TreeSitterProvider.list_symbols() returns correct symbols for representative TS, JS, and TSX files — including arrow functions, class methods, exported items, and nested scopes

- [x] **T02: Python + Rust + Go symbol extraction and full test suite** `est:1.5h`
  - Why: Completes the 6-language coverage. Each remaining language has distinct AST challenges: Python uses indentation for scope, Rust has impl blocks with optional trait bounds, Go uses receiver parameters for methods. These are lower risk because the architecture is proven by T01, but query accuracy still needs per-language verification.
  - Files: `src/parser.rs`, `tests/fixtures/sample.py`, `tests/fixtures/sample.rs`, `tests/fixtures/sample.go`
  - Do: Add Python query patterns (function_definition, class_definition, decorated functions, scope chain via parent walk). Add Rust query patterns (function_item, struct_item, enum_item, trait_item, impl_item methods with scope chain including type + optional trait). Add Go query patterns (function_declaration, method_declaration with field_identifier, type_spec for structs/interfaces). Write fixture files exercising each language's distinct constructs. Add unit tests verifying all 6 languages pass with correct symbol extraction.
  - Verify: `cargo test` — all language extraction tests pass; each language fixture produces correct symbol count, names, kinds, signatures, scope chains
  - Done when: TreeSitterProvider.list_symbols() returns correct symbols for representative Python, Rust, and Go files — including Python decorated methods with scope chains, Rust impl block methods with `TypeName.method` scope, Go receiver methods

## Files Likely Touched

- `Cargo.toml`
- `src/symbols.rs` (new)
- `src/parser.rs` (new)
- `src/language.rs`
- `src/lib.rs`
- `src/main.rs`
- `tests/fixtures/sample.ts` (new)
- `tests/fixtures/sample.tsx` (new)
- `tests/fixtures/sample.js` (new)
- `tests/fixtures/sample.py` (new)
- `tests/fixtures/sample.rs` (new)
- `tests/fixtures/sample.go` (new)
