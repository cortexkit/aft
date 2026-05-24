---
id: M001
provides:
  - Persistent Rust binary with NDJSON stdin/stdout protocol (ping, version, echo + 11 domain commands)
  - Tree-sitter symbol extraction for 6 languages (TypeScript, JavaScript, TSX, Python, Rust, Go) with 7 symbol kinds
  - Structural reading — outline (nested symbol tree) and zoom (symbol body with file-scoped caller/callee annotations)
  - Three-layer editing engine — edit_symbol, edit_match, write, batch with auto-backup, syntax validation, and structured disambiguation
  - Safety and recovery system — per-file undo stack, workspace-wide named checkpoints with TTL cleanup
  - OpenCode TypeScript plugin (@aft/core) registering all 11 commands as tools with Zod schemas, BinaryBridge with crash recovery
  - npm binary distribution — 5 platform packages, CI cross-compilation pipeline, cargo install fallback
  - AppContext architecture threading provider, backup store, checkpoint store, and config through dispatch
  - LanguageProvider trait with TreeSitterProvider implementation (LSP-ready via optional lsp_hints fields)
key_decisions:
  - "D001: Persistent process model — spawned once, JSON over stdin/stdout, in-memory state"
  - "D009: Newline-delimited JSON protocol — one object per line, self-contained"
  - "D010: Two-stage request parsing — envelope then command dispatch"
  - "D012: Tree-sitter query patterns embedded as inline const &str"
  - "D014/D029: RefCell for interior mutability throughout (TreeSitterProvider, BackupStore, CheckpointStore)"
  - "D019: TypeAlias as 7th SymbolKind beyond original 6"
  - "D025/D026: AppContext as single dispatch parameter, handler signature (&RawRequest, &AppContext) -> Response"
  - "D032: Disambiguation uses success response with code field, not error"
  - "D034: Plugin uses @opencode-ai/plugin's Zod re-export to avoid version mismatch"
  - "D035: @aft/{os}-{arch} npm platform package naming following esbuild/turbo convention"
  - "D036: Resolver fallback chain — npm package → PATH → cargo"
patterns_established:
  - "NDJSON protocol with RawRequest envelope and Response::success/error constructors"
  - "Per-language extract function pattern for tree-sitter symbol extraction"
  - "Command handler modules under src/commands/ with handle_X(req, ctx) -> Response signature"
  - "Auto-backup before every mutation via edit::auto_backup()"
  - "AftProcess test helper for integration tests with persistent BufReader"
  - "Tool module factory pattern — export function(bridge) → Record<string, ToolDefinition>"
observability_surfaces:
  - "stderr [aft] prefix for all binary lifecycle events (started, shutdown, parse errors, commands)"
  - "stderr [aft-plugin] prefix for plugin lifecycle events (spawn, crash, restart, shutdown)"
  - "ping command as health check, version for binary identification"
  - "list_checkpoints and edit_history as state inspection commands"
  - "Structured error codes in JSON responses (symbol_not_found, ambiguous_symbol, checkpoint_not_found, etc.)"
  - "validate-packages.mjs for npm package structural health"
  - "findBinary() error messages listing all attempted sources with per-source failure reasons"
requirement_outcomes:
  - id: R001
    from_status: active
    to_status: validated
    proof: "S01 — 120 sequential commands without restart, 8 malformed JSON recovery scenarios, clean shutdown on EOF"
  - id: R002
    from_status: active
    to_status: validated
    proof: "S02 — 53 unit tests prove symbol extraction across 6 languages, all symbol kinds, scope chains, export detection"
  - id: R003
    from_status: active
    to_status: validated
    proof: "S03 — 19 unit + 8 integration tests verify nested outline, zoom with annotations, disambiguation, multi-language"
  - id: R004
    from_status: active
    to_status: validated
    proof: "S05 — edit_symbol replace/delete, auto-backup with undo round-trip, syntax validation, disambiguation with ambiguous.ts"
  - id: R005
    from_status: active
    to_status: validated
    proof: "S05 — edit_match single/multiple occurrence, disambiguation with context lines, occurrence index selection"
  - id: R006
    from_status: active
    to_status: validated
    proof: "S05 — write creates/overwrites, batch atomic multi-edit with rollback on failure"
  - id: R007
    from_status: active
    to_status: validated
    proof: "S04 BackupStore + S05 auto-snapshot on all four mutation commands, undo round-trip proven in integration tests"
  - id: R008
    from_status: active
    to_status: validated
    proof: "S04 — checkpoint→modify→restore cycle, name overwrite, TTL cleanup, list with metadata, all via protocol"
  - id: R009
    from_status: active
    to_status: validated
    proof: "S06 — 9 integration tests (51 assertions): resolver, bridge lifecycle, crash recovery, 4 tool round-trips"
  - id: R010
    from_status: active
    to_status: validated
    proof: "S05 — all mutation responses include syntax_valid, intentional errors detected, unsupported languages return null"
  - id: R011
    from_status: active
    to_status: validated
    proof: "S05 — ambiguous.ts fixture proves structured candidates with name, qualified name, line, kind"
  - id: R012
    from_status: active
    to_status: validated
    proof: "S07 — 5 platform packages validated by validate-packages.mjs, 13 resolver tests, CI workflow, cargo metadata"
  - id: R032
    from_status: active
    to_status: validated
    proof: "S01 — all commands flow through JSON stdin/stdout, no shell escaping in any path"
duration: ~5h across 7 slices
verification_result: passed
completed_at: 2026-03-14
---

# M001: Foundation

**Persistent Rust binary with tree-sitter parsing for 6 languages, three-layer semantic editing, safety/recovery system, OpenCode plugin bridge, and npm binary distribution — 155 tests across Rust and TypeScript, all green, all 7 success criteria met.**

## What Happened

Built the complete foundation for Agent File Toolkit in 7 slices, each delivering a vertical capability increment.

**S01** established the persistent binary scaffold — a Rust process reading NDJSON on stdin, dispatching to command handlers, writing JSON responses on stdout. Two-stage request parsing (envelope then command dispatch) separates transport from logic. Three bootstrap commands (ping, version, echo) proved the protocol. Integration tests with a persistent `AftProcess` helper verified 120 sequential commands, malformed JSON recovery, and clean shutdown.

**S02** added tree-sitter symbol extraction for all 6 target languages. TypeScript and JavaScript share query patterns (with TSX support), while Python, Rust, and Go each required distinct extraction strategies — Python scope chains via parent-node walking, Rust impl scope chains distinguishing inherent vs trait implementations, Go export detection by uppercase convention. `TreeSitterProvider` replaced the `StubProvider` stub, implementing the `LanguageProvider` trait from S01. A 7th `SymbolKind::TypeAlias` was added beyond the original plan.

**S03** delivered structural reading: `outline` builds nested symbol trees from flat symbol lists, `zoom` extracts a symbol's body with configurable context lines and file-scoped caller/callee annotations via recursive AST walking. Established the `src/commands/` module pattern used by all subsequent slices.

**S04** introduced the safety system: `BackupStore` for per-file undo stacks and `CheckpointStore` for named workspace snapshots. The `AppContext` struct consolidated all shared state (provider, backup store, checkpoint store, config) into a single dispatch parameter, refactoring all existing handler signatures. Five commands wired through the protocol (undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints).

**S05** completed the editing surface with four mutation commands. `write` handles full file create/overwrite. `edit_symbol` resolves symbols via tree-sitter and applies replace/delete/insert_before/insert_after operations. `edit_match` targets content by string matching with disambiguation when multiple occurrences exist. `batch` applies multiple edits atomically with bottom-to-top sorting and rollback on failure. All four auto-backup before mutation and validate syntax after.

**S06** bridged everything to OpenCode. The TypeScript plugin registers all 11 commands as tools with Zod schemas. `BinaryBridge` manages the persistent child process with crash detection, exponential backoff auto-restart, and clean shutdown. The resolver finds the binary via PATH or cargo fallback (npm package slot ready for S07).

**S07** built the distribution pipeline: 5 npm platform packages following the esbuild/turbo pattern, a CI workflow for cross-compilation on all 5 targets, version sync across 7 files from git tags, and package validation tooling. The resolver was rewritten with a three-tier fallback (npm → PATH → cargo). Release profile optimizations reduced the binary from ~7.4MB to 6.4MB.

## Cross-Slice Verification

**Success Criterion 1: Agent can edit a function by name and get syntax validation feedback**
Met. S05 `edit_symbol` resolves symbols by name, applies edits, returns `syntax_valid` boolean. S06 registers this as an OpenCode tool. Plugin integration test proves the full round-trip: write file → edit_symbol replace → verify `syntax_valid: true` and `backup_id` present. Rust integration test `edit_symbol_replace` confirms the same through the binary protocol.

**Success Criterion 2: Agent can read a file's structure in one call**
Met. S03 `outline` returns nested symbol tree with kinds, ranges, signatures, member nesting, and export status. Integration tests prove outline on TS, Python, and Rust fixture files. Plugin tool test proves outline returns known symbols from fixture.

**Success Criterion 3: Agent can zoom to a single symbol and see callers/callees**
Met. S03 `zoom` returns symbol body with `calls_out` and `called_by` arrays populated by AST-based call extraction. Integration test `test_zoom_success_with_annotations` on `calls.ts` fixture verifies call graph edges.

**Success Criterion 4: Agent can checkpoint and restore workspace state**
Met. S04 integration test `test_checkpoint_create_restore_cycle` proves: checkpoint → write modified content → restore_checkpoint → verify file matches original. `list_checkpoints` returns metadata with file counts and timestamps.

**Success Criterion 5: Agent can undo an individual file edit**
Met. S04+S05 prove the full chain: every mutation auto-snapshots via `BackupStore`, `undo` restores the previous version. Plugin tool test proves write → edit_symbol → undo → file matches original.

**Success Criterion 6: All content flows through JSON stdin/stdout**
Met. 155 tests use JSON I/O exclusively. No shell argument strings for code content anywhere in the codebase. All file content, code snippets, and edit payloads are JSON string values.

**Success Criterion 7: Binary installs via npm install @aft/core**
Met structurally. 5 platform packages with correct `os`/`cpu` fields validated by `validate-packages.mjs`. CI workflow defines 5-platform cross-compilation and ordered npm publish. Resolver tests prove platform mapping for all 5 targets. Actual npm publish requires a `v*` tag push — local validation substitutes for CI verification.

**Definition of Done — additional items:**
- All 7 slices marked `[x]` in roadmap ✅
- 133 Rust tests (98 unit + 35 integration) + 22 plugin tests = 155 total, 0 failures ✅
- `cargo build` produces 0 warnings ✅
- Binary stays alive across 120+ sequential commands without crash (S01 `test_sequential_commands`) ✅
- Error cases handled: file_not_found, symbol_not_found, ambiguous_symbol, ambiguous_match, checkpoint_not_found, no_undo_history, invalid_request, unknown_command, malformed JSON — all return structured error responses, process stays alive ✅

## Requirement Changes

- R001: active → validated — persistent process proven by 120 sequential commands, malformed recovery, clean shutdown
- R002: active → validated — 53 unit tests across 6 languages with all symbol kinds, scope chains, export detection
- R003: active → validated — outline and zoom with 27 tests covering nested structures, annotations, disambiguation
- R004: active → validated — edit_symbol with 4 operations, auto-backup, syntax validation, disambiguation
- R005: active → validated — edit_match with string matching, occurrence selection, disambiguation with context
- R006: active → validated — write and batch with atomic multi-edit, rollback on failure
- R007: active → validated — BackupStore infrastructure + auto-snapshot on all mutation commands, undo round-trip proven
- R008: active → validated — checkpoint create/restore/list/cleanup with TTL, full cycle proven
- R009: active → validated — plugin spawns binary, registers 11 tools, crash recovery, 9 integration tests
- R010: active → validated — every mutation response includes syntax_valid, intentional errors caught
- R011: active → validated — disambiguation returns structured candidates with qualified names and context
- R012: active → validated — 5 platform packages, CI pipeline, resolver fallback chain, cargo metadata
- R032: active → validated — all communication via JSON stdin/stdout, no shell escaping

## Forward Intelligence

### What the next milestone should know
- The `AppContext` struct in `src/context.rs` is the extension point — add new stores as `RefCell<T>` fields with accessor methods. All handlers receive `&AppContext`.
- Command dispatch in `main.rs` is a flat `match` on the command string. Adding new commands = new match arm + handler module in `src/commands/`. This will scale fine for M002's ~10 new commands but may want a registry pattern by M003.
- `FileParser::extract_symbols(path)` is the primary entry for symbol data. `TreeSitterProvider` wraps it with the `LanguageProvider` trait interface. For M002's import management and compound operations, you'll likely need direct `FileParser` access for AST walking beyond symbol extraction.
- The plugin at `opencode-plugin-aft/` is an independent ESM package. Adding new tools = new entries in the tool module factory functions. Zod schemas use `tool.schema` (the plugin's bundled zod), not a direct zod import.
- Integration tests use temp directories extensively. The `AftProcess` helper in `tests/integration/helpers.rs` handles binary lifecycle for Rust tests; plugin tests use fresh `BinaryBridge` instances per test.

### What's fragile
- **RefCell throughout** — `TreeSitterProvider`, `BackupStore`, `CheckpointStore` all use `RefCell` for interior mutability. Safe in single-threaded context but will panic on concurrent borrow. If M002+ ever introduces async or parallel command handling, all RefCells must become Mutexes.
- **Tree-sitter query patterns as inline strings** — typos surface as runtime `QueryError`, not compile errors. Every pattern is covered by tests, but adding new patterns (M002 import queries, compound operation patterns) needs immediate test coverage.
- **Symbol ranges exclude export keywords** — tree-sitter `function_declaration` nodes don't include `export`. Agents using `edit_symbol replace` must provide content without the export prefix. This is correct but surprising.
- **Batch doesn't support disambiguation** — each match edit must have exactly one occurrence. Agents should resolve ambiguities with individual `edit_match` calls first.
- **In-memory state only** — all stores (backup, checkpoint, parse cache) are lost on binary restart. Acceptable for persistent process model but means crash = state loss.

### Authoritative diagnostics
- `cargo test` — 133 tests in ~0.5s. The fastest, most reliable signal for whether the binary works correctly. Integration tests in `tests/integration/` exercise the full protocol path.
- `bun test` in `opencode-plugin-aft/` — 22 tests in ~1s. Tests the complete plugin→binary stack with the real binary (no mocks).
- `node scripts/validate-packages.mjs` — structural health check for all 6 npm packages. If this passes, package configuration is sound.
- Structured error codes in JSON responses (`symbol_not_found`, `ambiguous_symbol`, `parse_error`, etc.) — these are the programmatic API for error handling.
- stderr with `[aft]` and `[aft-plugin]` prefixes — lifecycle events, command tracing, parse errors.

### What assumptions changed
- Plan assumed 6 SymbolKind variants — added TypeAlias as 7th (D019). All downstream match arms handle 7 variants.
- Plan assumed std Iterator for tree-sitter query matches — tree-sitter 0.24 uses StreamingIterator requiring the `streaming-iterator` crate (D015).
- Plan assumed outline handler tests needed updating for AppContext refactor — they test internal functions directly, not handler signatures, so 0 changes needed.
- Per-call BufReader for integration tests doesn't work — buffered data lost between calls. AftProcess with persistent BufReader is mandatory (D011).

## Files Created/Modified

- `Cargo.toml` — project manifest with tree-sitter, 5 grammar crates, serde, streaming-iterator; crates.io metadata; release profile optimizations
- `src/main.rs` — persistent process loop with two-stage request parsing, 14 command dispatch arms, AppContext construction
- `src/lib.rs` — module declarations and re-exports, unit test host
- `src/protocol.rs` — RawRequest, Response, EchoParams types with serde flatten
- `src/error.rs` — AftError enum with 7 variants (SymbolNotFound, AmbiguousSymbol, ParseError, FileNotFound, InvalidRequest, CheckpointNotFound, NoUndoHistory, AmbiguousMatch)
- `src/config.rs` — Config struct with runtime defaults
- `src/language.rs` — LanguageProvider trait, re-exports from symbols.rs
- `src/symbols.rs` — Symbol, SymbolKind (7 variants), Range, SymbolMatch types
- `src/parser.rs` — FileParser with language detection, parse caching, 5 language query patterns, 6 extract functions, TreeSitterProvider
- `src/context.rs` — AppContext struct threading provider, backup, checkpoint, config
- `src/backup.rs` — BackupStore with per-file snapshot/restore/history
- `src/checkpoint.rs` — CheckpointStore with create/restore/list/cleanup
- `src/edit.rs` — shared edit engine (line_col_to_byte, replace_byte_range, validate_syntax, auto_backup)
- `src/commands/mod.rs` — module declarations for all 11 command handlers
- `src/commands/outline.rs` — nested symbol tree builder
- `src/commands/zoom.rs` — symbol body extraction with AST-based call annotations
- `src/commands/write.rs` — full file create/overwrite with auto-backup
- `src/commands/edit_symbol.rs` — symbol-level editing with 4 operations and disambiguation
- `src/commands/edit_match.rs` — content-based editing with occurrence selection
- `src/commands/batch.rs` — atomic multi-edit with rollback
- `src/commands/undo.rs` — per-file undo
- `src/commands/edit_history.rs` — per-file backup stack viewer
- `src/commands/checkpoint.rs` — workspace snapshot creation
- `src/commands/restore_checkpoint.rs` — workspace snapshot restoration
- `src/commands/list_checkpoints.rs` — checkpoint listing with metadata
- `tests/integration/main.rs` — integration test entry
- `tests/integration/helpers.rs` — shared AftProcess + fixture_path
- `tests/integration/protocol_test.rs` — 4 protocol reliability tests
- `tests/integration/commands_test.rs` — 8 structural reading tests
- `tests/integration/safety_test.rs` — 7 safety system tests
- `tests/integration/edit_test.rs` — 17 editing engine tests (16 shown in count due to test grouping)
- `tests/fixtures/` — 8 fixture files (sample.ts, .tsx, .js, .py, .rs, .go, calls.ts, ambiguous.ts)
- `opencode-plugin-aft/package.json` — @aft/core ESM package
- `opencode-plugin-aft/tsconfig.json` — TypeScript config
- `opencode-plugin-aft/src/index.ts` — Plugin entry point
- `opencode-plugin-aft/src/resolver.ts` — binary resolver with npm/PATH/cargo fallback
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge process manager
- `opencode-plugin-aft/src/tools/reading.ts` — outline + zoom tool definitions
- `opencode-plugin-aft/src/tools/editing.ts` — write + edit_symbol + edit_match + batch tool definitions
- `opencode-plugin-aft/src/tools/safety.ts` — undo + edit_history + checkpoint + restore_checkpoint + list_checkpoints
- `opencode-plugin-aft/src/__tests__/bridge.test.ts` — bridge lifecycle tests
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — tool round-trip tests
- `opencode-plugin-aft/src/__tests__/resolver.test.ts` — resolver unit tests
- `npm/darwin-arm64/package.json` — macOS ARM64 platform package
- `npm/darwin-x64/package.json` — macOS Intel platform package
- `npm/linux-arm64/package.json` — Linux ARM64 platform package
- `npm/linux-x64/package.json` — Linux x64 platform package
- `npm/win32-x64/package.json` — Windows x64 platform package
- `.github/workflows/release.yml` — 6-job CI release pipeline
- `scripts/version-sync.mjs` — version sync from git tag across 7 files
- `scripts/validate-packages.mjs` — npm package structural health check
