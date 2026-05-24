---
id: T01
parent: S02
milestone: M001
provides:
  - SymbolKind enum and expanded Symbol struct with Serialize
  - FileParser with language detection (6 extensions) and parse tree caching
  - TreeSitterProvider implementing LanguageProvider trait
  - TS/JS/TSX query patterns for symbol extraction
  - Test fixtures for 3 web languages
key_files:
  - src/symbols.rs
  - src/parser.rs
  - src/language.rs
key_decisions:
  - Used RefCell<FileParser> inside TreeSitterProvider for interior mutability — LanguageProvider trait takes &self but parsing needs &mut for cache updates
  - streaming-iterator crate added as explicit dep — tree-sitter 0.24 QueryMatches implements StreamingIterator not std Iterator
  - Arrow functions classified as SymbolKind::Function (not a separate ArrowFunction kind) — consistent with how they appear in symbol lists
  - TypeAlias added to SymbolKind beyond the plan's 6 variants — TS type aliases are common enough to warrant first-class treatment
  - Export detection via byte-range containment check against export_statement nodes — simpler than walking parent chains
patterns_established:
  - StreamingIterator advance()/get() pattern for tree-sitter query matches
  - Per-language extract function (extract_ts_symbols, extract_js_symbols) called from FileParser::extract_symbols — keeps language-specific logic isolated
  - Fixture-based testing pattern: tests/fixtures/sample.{ext} with TreeSitterProvider as the test entry point
observability_surfaces:
  - FileParser logs parse failures to stderr with [aft] prefix
  - AftError::ParseError for tree-sitter failures — visible in JSON responses
  - AftError::InvalidRequest for unsupported extensions — includes the rejected extension
duration: ~45min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Parser infrastructure + TS/JS/TSX symbol extraction

**Tree-sitter parsing foundation with TS/JS/TSX symbol extraction — 36 unit tests + 4 integration tests all green, 0 warnings.**

## What Happened

Added tree-sitter 0.24 + 5 grammar crates (typescript, javascript, python, rust, go) and streaming-iterator to Cargo.toml. All compile without version conflicts.

Created `src/symbols.rs` with `SymbolKind` enum (Function, Class, Method, Struct, Interface, Enum, TypeAlias), expanded `Symbol` struct (name, kind, range, signature, scope_chain, exported, parent), and `SymbolMatch` — all with Serialize derives.

Created `src/parser.rs` with:
- `LangId` enum and `detect_language()` mapping 7 extensions (.ts, .tsx, .js, .jsx, .py, .rs, .go) to 6 language IDs
- `FileParser` with HashMap-based parse tree cache using mtime invalidation
- TS query pattern (shared with TSX): captures function_declaration, arrow_function assigned to const/let/var, class_declaration + method_definition, interface_declaration, enum_declaration, type_alias_declaration, export_statement wrappers
- JS query pattern: same minus TS-specific constructs (interface, enum, type alias)
- `TreeSitterProvider` implementing `LanguageProvider` trait with `RefCell<FileParser>` for interior mutability

Updated `src/language.rs` to re-export Symbol, Range, SymbolMatch from symbols.rs. Updated `src/lib.rs` with new module declarations. Wired `TreeSitterProvider` into `src/main.rs`.

Created test fixtures: `sample.ts` (function, arrow fn, class with methods, interface, enum, type alias, non-exported function), `sample.tsx` (React component as arrow fn, class component, regular function), `sample.js` (function, arrow fn, class with methods, default export, non-exported arrow fn).

## Verification

- `cargo build` — 0 errors, 0 warnings ✅
- `cargo test` — 40 tests pass (36 unit + 4 integration) ✅
- TS: 7+ symbols extracted (greet, add, UserService, getUser, addUser, Config, Status, UserId, internalHelper) with correct kinds ✅
- TS export detection: greet/add/UserService/Config/Status exported, internalHelper not ✅
- TS scope chain: methods have ["UserService"] scope, parent set to "UserService" ✅
- TS signatures: present and contain function names ✅
- JS: 4+ symbols (multiply, divide, EventEmitter, main, internalUtil) with correct kinds ✅
- JS arrow fn: divide correctly named as Function, internalUtil correctly non-exported ✅
- TSX: Button (arrow fn), Counter (class), formatLabel (function) extracted without JSX choking ✅
- Failure path: unsupported extension returns InvalidRequest with "unsupported file extension" + extension name ✅
- resolve_symbol: finds exact match, returns SymbolNotFound for missing names ✅
- Parse cache: second parse of same file returns identical tree root ✅

**Slice-level verification (partial — T02 pending):**
- ✅ `cargo build` — 0 warnings, all grammar crates compile
- ✅ `cargo test` — all tests pass
- ✅ TS/JS/TSX extraction tests with correct counts, names, kinds, ranges, signatures, scope chains, exports
- ✅ Fixture files: sample.ts, sample.tsx, sample.js
- ✅ Failure-path test for unsupported extension
- ⬜ sample.py, sample.rs, sample.go fixtures — T02
- ⬜ Python/Rust/Go extraction tests — T02

## Diagnostics

- Parse failures: logged to stderr with `[aft]` prefix, e.g. `[aft] parse failed for /path/to/file`
- Unsupported extensions: `AftError::InvalidRequest { message: "unsupported file extension: xyz" }` — surfaces as `{ "code": "invalid_request", "message": "..." }` in JSON responses
- Grammar init failures: logged with `[aft] grammar init failed for {lang}: {error}`

## Deviations

- Added `TypeAlias` to `SymbolKind` — not in original plan's 6 variants, but TS type aliases are ubiquitous and the TS query already matches them
- Added `streaming-iterator = "0.1"` to Cargo.toml — tree-sitter 0.24's `QueryMatches` implements `StreamingIterator` (not std `Iterator`), requiring this dependency for the advance/get iteration pattern
- Used `RefCell<FileParser>` inside `TreeSitterProvider` instead of the plan's simpler struct wrapper — necessary because `LanguageProvider` trait methods take `&self` but parsing needs mutable cache access

## Known Issues

None.

## Files Created/Modified

- `Cargo.toml` — added tree-sitter 0.24, 5 grammar crates, streaming-iterator
- `src/symbols.rs` — new: SymbolKind, Symbol, Range, SymbolMatch with Serialize
- `src/parser.rs` — new: FileParser, TreeSitterProvider, TS/JS query patterns, 20 unit tests
- `src/language.rs` — evolved: re-exports Symbol/Range/SymbolMatch from symbols.rs, keeps trait + StubProvider
- `src/lib.rs` — updated: added symbols, parser module declarations
- `src/main.rs` — updated: instantiates TreeSitterProvider
- `tests/fixtures/sample.ts` — new: representative TS with all symbol kinds
- `tests/fixtures/sample.tsx` — new: React component + TS constructs
- `tests/fixtures/sample.js` — new: representative JS with arrow functions
- `.gsd/milestones/M001/slices/S02/S02-PLAN.md` — added Observability/Diagnostics section and failure-path verification
