---
id: S02
parent: M001
milestone: M001
provides:
  - SymbolKind enum (Function, Class, Method, Struct, Interface, Enum, TypeAlias) with Serialize
  - Symbol struct with name, kind, range, signature, scope_chain, exported, parent
  - FileParser with extension-based language detection (7 extensions → 6 languages), parse tree caching with mtime invalidation
  - TreeSitterProvider implementing LanguageProvider trait (replaces StubProvider)
  - Tree-sitter query patterns for all 6 languages — TS (shared with TSX), JS, Python, Rust, Go
  - 6 test fixture files exercising all symbol kinds per language
requires:
  - slice: S01
    provides: LanguageProvider trait, AftError types, protocol types, main.rs process loop
affects:
  - S03 (outline/zoom consume FileParser + Symbol types)
  - S05 (edit_symbol consumes symbol resolution via TreeSitterProvider)
key_files:
  - src/symbols.rs
  - src/parser.rs
  - src/language.rs
  - tests/fixtures/sample.ts
  - tests/fixtures/sample.tsx
  - tests/fixtures/sample.js
  - tests/fixtures/sample.py
  - tests/fixtures/sample.rs
  - tests/fixtures/sample.go
key_decisions:
  - D012: Query patterns embedded as inline const &str in parser.rs
  - D013: Symbol/SymbolKind/Range in symbols.rs, trait stays in language.rs
  - D014: RefCell<FileParser> inside TreeSitterProvider for interior mutability
  - D015: streaming-iterator crate for tree-sitter 0.24 QueryMatches iteration
  - D016: Rust traits mapped to SymbolKind::Interface
  - D017: Rust impl scope chains — inherent ["Type"], trait ["Trait for Type"]
  - D018: Python/Go scope chains via parent-node walking (not query captures)
  - D019: TypeAlias added as 7th SymbolKind variant beyond plan
  - D020: Export detection per language — TS/JS byte-range containment in export_statement, Rust pub modifier, Go uppercase, Python always false
patterns_established:
  - Per-language extract function pattern (extract_ts_symbols, extract_js_symbols, extract_py_symbols, extract_rs_symbols, extract_go_symbols) — isolated language-specific logic
  - StreamingIterator advance()/get() for tree-sitter query matches
  - Fixture-based testing with TreeSitterProvider as entry point
  - Scope chain construction via parent-node walking for languages without query-expressible ancestor relationships
observability_surfaces:
  - FileParser logs parse failures to stderr with [aft] prefix
  - AftError::ParseError for tree-sitter failures — JSON code "parse_error"
  - AftError::InvalidRequest for unsupported extensions — JSON code "invalid_request" with extension name
  - Symbol.scope_chain and Symbol.exported in JSON output for agent inspection
drill_down_paths:
  - .gsd/milestones/M001/slices/S02/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S02/tasks/T02-SUMMARY.md
duration: ~2h
verification_result: passed
completed_at: 2026-03-14
---

# S02: Tree-sitter Multi-Language Engine

**Tree-sitter symbol extraction for 6 languages — 53 unit tests + 4 integration tests, all green, 0 build warnings.**

## What Happened

Built the tree-sitter parsing infrastructure and symbol extraction engine in two tasks.

**T01** established the architecture: added tree-sitter 0.24 + 5 grammar crates + streaming-iterator to Cargo.toml. Created `symbols.rs` with `SymbolKind` enum and `Symbol` struct. Created `parser.rs` with `FileParser` (language detection for 7 extensions, parse tree caching with mtime invalidation, query execution engine) and `TreeSitterProvider` implementing the `LanguageProvider` trait from S01. Wrote TS query patterns (shared with TSX) and JS query patterns covering function declarations, arrow functions assigned to const/let/var, class methods, interfaces, enums, type aliases, and export statement wrappers. Key challenge: tree-sitter 0.24's `QueryMatches` uses `StreamingIterator` (not std `Iterator`), requiring the streaming-iterator crate and advance()/get() pattern. Interior mutability via `RefCell<FileParser>` needed because `LanguageProvider` trait takes `&self` but parsing needs `&mut` for cache updates.

**T02** added Python, Rust, and Go support. Each language has distinct extraction challenges: Python scope chains built by parent-node walking (queries can't express ancestor relationships); Rust impl methods extracted by walking `impl_item` children (queries can't relate a function to its grandparent impl type), with scope chains distinguishing inherent (`["MyStruct"]`) vs trait (`["Drawable for MyStruct"]`) implementations; Go receiver types extracted via recursive type_identifier search, with export detection by uppercase first character. All three languages follow the per-language extract function pattern established in T01.

The `TreeSitterProvider` replaces `StubProvider` in `main.rs`. All language-specific logic is isolated in extract functions, making each language independently testable and modifiable.

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test` — 57 tests total (53 unit + 4 integration), all pass ✅
- **TypeScript**: 9 symbols extracted (greet, add, UserService, getUser, addUser, Config, Status, UserId, internalHelper). Correct kinds, export detection (internalHelper not exported), scope chains (methods → ["UserService"]), signatures present. ✅
- **TSX**: 3 symbols (Button, Counter, formatLabel) extracted without JSX breaking parser. ✅
- **JavaScript**: 5 symbols (multiply, divide, EventEmitter, main, internalUtil). Arrow fn correctly named, non-exported items detected. ✅
- **Python**: 9 symbols including nested class. Method scope chain ["MyClass"], nested scope ["OuterClass", "InnerClass"], decorated function signature includes decorator text. ✅
- **Rust**: 9 symbols. Inherent impl scope ["MyStruct"], trait impl scope ["Drawable for MyStruct"], pub export detection. ✅
- **Go**: 6 symbols. Receiver method scope ["MyStruct"], uppercase export detection. ✅
- **Cross-language**: all 6 fixture files produce ≥2 symbols each ✅
- **Failure path**: unsupported extension returns `AftError::InvalidRequest` with "unsupported file extension" ✅
- **Parse cache**: second parse returns same tree root, mtime invalidation works ✅
- **resolve_symbol**: finds exact match, returns `SymbolNotFound` for missing names ✅
- All S01 integration tests (sequential commands, malformed recovery, shutdown) still pass ✅

## Requirements Advanced

- R002 (Multi-language tree-sitter parsing) — fully implemented: 6 languages, auto-detection from extension, symbol extraction queries per language
- R034 (Web-first language priority) — followed: TS/JS/TSX in T01, Python/Rust/Go in T02

## Requirements Validated

- R002 — 57 tests prove symbol extraction works correctly across all 6 languages with representative code patterns. All symbol kinds, scope chains, export detection, and edge cases (arrow functions, impl blocks, receiver methods, decorated functions) verified.

## New Requirements Surfaced

None.

## Requirements Invalidated or Re-scoped

None.

## Deviations

- Added `TypeAlias` as 7th `SymbolKind` variant — not in the plan's 6 (Function, Class, Method, Struct, Interface, Enum). TS type aliases are ubiquitous enough to warrant first-class treatment. Downstream consumers (S03 outline, S05 edit_symbol) benefit from distinguishing type aliases.
- Added `streaming-iterator = "0.1"` dependency — not planned, but required because tree-sitter 0.24's `QueryMatches` implements `StreamingIterator` rather than std `Iterator`.

## Known Limitations

- Symbol extraction is purely structural — no type information, no cross-file resolution. A function's return type is in the signature string but not parsed semantically.
- Python export detection always returns `false` — Python has no syntactic export marker. This is correct behavior but means downstream outline/zoom won't distinguish "public API" in Python files.
- Query patterns are inline `const &str` in parser.rs. If patterns grow significantly, extracting to `.scm` files would improve maintainability.
- Parse tree cache is in-memory only — lost on binary restart. Acceptable for M001; persistence deferred (R037).

## Follow-ups

None — all planned work complete.

## Files Created/Modified

- `Cargo.toml` — added tree-sitter 0.24, 5 grammar crates, streaming-iterator
- `src/symbols.rs` — new: SymbolKind, Symbol, Range, SymbolMatch with Serialize
- `src/parser.rs` — new: FileParser, TreeSitterProvider, 5 language query patterns, 6 extract functions, 33 unit tests
- `src/language.rs` — evolved: re-exports Symbol/Range/SymbolMatch from symbols.rs
- `src/lib.rs` — updated: added symbols, parser module declarations
- `src/main.rs` — updated: TreeSitterProvider replaces StubProvider
- `tests/fixtures/sample.ts` — new: TS fixture (9 symbols, all kinds)
- `tests/fixtures/sample.tsx` — new: TSX fixture (React components + TS constructs)
- `tests/fixtures/sample.js` — new: JS fixture (arrow functions, classes, exports)
- `tests/fixtures/sample.py` — new: Python fixture (classes, decorators, nested classes)
- `tests/fixtures/sample.rs` — new: Rust fixture (impl blocks, traits, pub/private)
- `tests/fixtures/sample.go` — new: Go fixture (receiver methods, exported/unexported)

## Forward Intelligence

### What the next slice should know
- `TreeSitterProvider` is the active provider in `main.rs`. Call `list_symbols(file_path)` to get all symbols, `resolve_symbol(file_path, name)` for exact lookup.
- `FileParser::extract_symbols(file_path)` is the lower-level entry if you need the parser directly (e.g., for re-parse after edit in S05).
- Symbol ranges use 1-based line numbers (`range.start_line`, `range.end_line`). These are byte-offset-derived but converted to line numbers at extraction time.
- The `signature` field contains the declaration line text (e.g., `pub fn new(name: String) -> Self`). For Python decorated functions, it includes the decorator line.

### What's fragile
- Tree-sitter query patterns are string constants compiled at first use — a typo in a query pattern will surface as a runtime `QueryError`, not a compile-time error. All current patterns are covered by tests, but adding new patterns needs test coverage.
- `RefCell<FileParser>` — safe in single-threaded context but will panic on concurrent borrow. If the binary ever goes multi-threaded, switch to `Mutex`.

### Authoritative diagnostics
- `cargo test --lib -- parser::tests` — runs all 33 parser unit tests, fastest way to verify symbol extraction after changes
- `AftError` JSON responses with `code` field — `"parse_error"` for tree-sitter failures, `"invalid_request"` for unsupported extensions, `"symbol_not_found"` for missing symbols

### What assumptions changed
- Plan assumed 6 SymbolKind variants — added TypeAlias as 7th. Downstream slices should handle all 7 variants in match arms.
- Plan assumed std Iterator for query matches — tree-sitter 0.24 uses StreamingIterator. Pattern is established and documented in T01.
