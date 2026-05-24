# M004: Refactoring Primitives

**Vision:** Three single-call refactoring operations (move symbol, extract function, inline symbol) that replace the most error-prone multi-step agent workflows, plus LSP integration through the plugin mediation layer for enhanced symbol resolution accuracy.

## Success Criteria

- Agent moves a function from one file to another with a single `move_symbol` call — all import statements across the workspace are updated, source file's export is removed, destination file has the symbol with correct exports, no broken references
- Agent extracts a 15-line code block with 3 free variables into a new function via `extract_function` — parameters are correctly inferred, return value detected, original range replaced with a call to the new function
- Agent inlines a single-return function call via `inline_symbol` — call site is replaced with the function body, argument-to-parameter substitution is correct, scope conflicts are detected and reported
- With LSP hints provided via the plugin, `edit_symbol` resolves an ambiguous symbol that tree-sitter alone returns as ambiguous — disambiguation uses workspace symbol location data from the language server

## Key Risks / Unknowns

- **Import rewiring completeness** — move_symbol must find ALL files that import the moved symbol. callers_of gives the consumer list, but barrel re-exports, aliased imports, and `require()` calls could be missed. A single missed consumer means a broken codebase.
- **Free variable classification** — extract_function must distinguish local variables (become parameters), module-level bindings (don't become parameters), and `this`/`self` references (require method extraction). Getting this wrong produces incorrect function signatures.
- **Relative path computation** — when rewriting import paths after a move, the new path must be relative from each consumer file's directory to the destination file. Getting parent-walking wrong produces `import from '../../../wrong/path'`.
- **OpenCode SDK LSP surface narrower than planned** — research found only `find.symbols()`, `find.text()`, and `lsp.status()` — no go-to-definition or find-references. LSP integration will be workspace symbol verification, not full protocol access.

## Proof Strategy

- Import rewiring completeness → retire in S01 by proving move_symbol updates imports in 3+ consumer files including one that uses an aliased import, verified through integration tests with multi-file fixtures
- Free variable classification → retire in S02 by proving extract_function correctly infers parameters from a block containing local refs, module-level refs, and `this` — verified through integration tests
- Relative path computation → retire in S01 by proving import paths are correct when source, destination, and consumer files are in different directory depths — verified through unit tests on the path computation utility
- SDK LSP surface → retire in S03 by proving workspace symbol disambiguation works through the real `lsp_hints` field and falls back cleanly when LSP data is unavailable

## Verification Classes

- Contract verification: integration tests through binary protocol (following M001-M003 pattern — 368 Rust tests + 39 plugin tests as baseline), unit tests for path computation and free variable detection
- Integration verification: plugin tool round-trip tests via `bun test` for all 3 new tools (aft_move_symbol, aft_extract_function, aft_inline_symbol) + LSP hint flow
- Operational verification: none (no new operational concerns beyond M001-M003's persistent process)
- UAT / human verification: none

## Milestone Definition of Done

This milestone is complete only when all are true:

- move_symbol correctly moves a function from a service file to a utils file in a multi-file fixture — all importing files are updated, no broken references, verified by integration tests
- extract_function extracts a code block with 3 free variables into a new function — parameters, return type, and call site replacement are all correct, verified by integration tests across TS/JS/Python
- inline_symbol replaces a function call with the body — argument substitution correct, scope conflicts detected and reported, verified by integration tests
- With lsp_hints populated, edit_symbol resolves an ambiguous symbol that tree-sitter alone couldn't disambiguate, verified by integration test
- All 3 new refactoring commands support dry_run mode (D071 — raw diff preview)
- All 3 new commands auto-format and validate via write_format_validate (D046, D066)
- All new plugin tools registered with Zod schemas and tested via bun test
- `cargo test` passes (baseline 368 + new tests)
- `bun test` passes (baseline 39 + new tests)

## Requirement Coverage

- Covers: R028 (move symbol), R029 (extract function), R030 (inline symbol), R033 (LSP integration)
- Partially covers: R031 (LSP-aware architecture — completing the provider interface from M001)
- Leaves for later: R034 (web-first priority — maintained throughout but formal validation deferred), R035 (multi-language files — deferred), R036 (extensible templates — deferred), R037 (call graph persistence — deferred)
- Orphan risks: none — all active requirements relevant to M004 are mapped

## Slices

- [x] **S01: Move Symbol with Import Rewiring** `risk:high` `depends:[]`
  > After this: agent calls `aft_move_symbol` to move a function from one file to another — all import statements across the workspace are updated automatically, verified by integration tests with multi-file fixtures spanning 5+ files
- [x] **S02: Extract Function & Inline Symbol** `risk:medium` `depends:[]`
  > After this: agent calls `aft_extract_function` to extract a code range into a new function with auto-detected parameters and return type, and `aft_inline_symbol` to replace a function call with its body — both verified by integration tests across TS/JS/Python
- [x] **S03: LSP-Enhanced Symbol Resolution** `risk:low` `depends:[S01,S02]`
  > After this: when the plugin provides LSP workspace symbol data via lsp_hints, edit_symbol and the refactoring commands resolve ambiguous symbols with higher accuracy — verified by integration tests with mock lsp_hints data and plugin round-trip tests

## Boundary Map

### S01 → S02

Produces:
- `move_symbol` command handler in `src/commands/move_symbol.rs` following the `handle_*(req, ctx)` pattern (D026)
- Relative path computation utility (computing correct import paths between arbitrary file locations)
- Multi-file mutation coordination pattern using auto-checkpoint + sequential write_format_validate
- Plugin tool `aft_move_symbol` with Zod schema

Consumes:
- `CallGraph::callers_of()` for consumer discovery (M003)
- `imports::parse_imports()`, `find_insertion_point()`, `generate_import_line()`, `is_duplicate()` for import rewiring (M002)
- `edit.rs::write_format_validate()` for mutation tail (M002)
- `BackupStore::checkpoint()` for pre-operation safety (M001)

### S01 → S03

Produces:
- Same as S01 → S02 — move_symbol is a consumer of LSP hints (S03 enhances its resolution)

### S02 → S03

Produces:
- `extract_function` command handler in `src/commands/extract_function.rs`
- `inline_symbol` command handler in `src/commands/inline_symbol.rs`
- Free variable detection utility (AST walking for variable references vs declarations in scope)
- Scope conflict detection utility (checking for variable name collisions)
- Plugin tools `aft_extract_function` and `aft_inline_symbol` with Zod schemas

Consumes:
- `parser.rs::extract_symbols()`, `detect_language()`, `grammar_for()` for AST analysis (M001)
- `edit.rs::write_format_validate()` for mutation tail (M002)
- `BackupStore` for undo (M001)

### S03 (terminal)

Produces:
- LSP-enhanced `LanguageProvider` resolution path in the binary
- Plugin LSP query infrastructure using OpenCode SDK's `client.find.symbols()`
- `lsp_hints` population logic in plugin bridge for refactoring and editing commands

Consumes:
- All existing command handlers (enhanced with LSP fallback resolution)
- Plugin bridge `BinaryBridge.send()` — lsp_hints flows through existing `params` JSON (D003)
- OpenCode SDK `client.find.symbols()`, `client.lsp.status()`
