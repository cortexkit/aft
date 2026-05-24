# M001: Foundation

**Vision:** Give AI coding agents a Rust-powered file toolkit that replaces the read-grep-edit-read cycle with semantic operations — edit by symbol name, read file structure, checkpoint/restore — all through an OpenCode plugin backed by a persistent binary process using tree-sitter for 6 languages.

## Success Criteria

- Agent can edit a function by name through OpenCode and get syntax validation feedback without knowing line numbers
- Agent can read a file's structure (outline) in one call instead of reading the entire file
- Agent can zoom to a single symbol and see what calls it and what it calls
- Agent can checkpoint workspace state, make experimental changes, and restore to the checkpoint
- Agent can undo an individual file edit in one call
- All content flows through JSON stdin/stdout — zero shell escaping errors
- Binary installs via `npm install @aft/core` on macOS (ARM/Intel), Linux (ARM/x64), and Windows (x64)

## Key Risks / Unknowns

- **Tree-sitter symbol extraction accuracy** — query patterns must correctly identify functions, classes, methods, structs across 6 languages with different scope rules. TS/JS/TSX share patterns, but Python (indent scope), Rust (impl blocks), and Go (receiver methods) each need distinct handling.
- **Persistent process reliability** — the binary must stay alive, handle rapid sequential commands, recover from malformed input, and not leak memory over long sessions.
- **Cross-compilation with embedded grammars** — tree-sitter grammar C code must compile and link correctly on all 5 target platforms.

## Proof Strategy

- Tree-sitter accuracy → retire in S02 by proving symbol extraction works correctly on real-world files across all 6 languages with a test suite of representative code patterns
- Persistent process reliability → retire in S01 by proving the binary handles 1000+ sequential commands, malformed JSON recovery, and graceful shutdown
- Cross-compilation → retire in S07 by proving CI builds and tests pass on all 5 platforms

## Verification Classes

- Contract verification: Rust unit tests per command, integration tests for command sequences, tree-sitter query accuracy tests per language
- Integration verification: OpenCode plugin spawns binary, registers tools, agent completes a full edit workflow through tool calls
- Operational verification: binary stays alive across commands, restarts after crash, checkpoint store persists to disk
- UAT / human verification: agent in a real OpenCode session uses AFT tools for a real editing task

## Milestone Definition of Done

This milestone is complete only when all are true:

- All 7 slices are complete and verified
- An agent in OpenCode can outline → zoom → edit_symbol → verify syntax in a single conversation
- Checkpoint/restore round-trips correctly (checkpoint, edit, restore, verify)
- Binary stays alive across 100+ sequential tool calls without memory leak or crash
- `npm install @aft/core` installs the correct platform binary on at least macOS ARM
- All M001 commands handle error cases gracefully (file not found, ambiguous symbol, malformed JSON)

## Requirement Coverage

- Covers: R001, R002, R003, R004, R005, R006, R007, R008, R009, R010, R011, R012, R031, R032, R034
- Partially covers: none
- Leaves for later: R013–R030, R033
- Orphan risks: none

## Slices

- [x] **S01: Binary Scaffold & Persistent Protocol** `risk:high` `depends:[]`
  > After this: binary starts as a persistent process, accepts JSON commands on stdin (ping, version, echo), responds on stdout, stays alive between commands. Verified by integration test sending 100+ commands sequentially.

- [x] **S02: Tree-sitter Multi-Language Engine** `risk:high` `depends:[S01]`
  > After this: binary parses files in all 6 languages and extracts symbols (functions, classes, methods, structs, interfaces, enums) with names, ranges, signatures, scope chains, and export status. Verified by test suite against representative code files per language.

- [x] **S03: Structural Reading** `risk:medium` `depends:[S02]`
  > After this: `outline` returns a file's full symbol structure and `zoom` returns a single symbol's body with caller/callee annotations. Verified by running outline and zoom on real multi-symbol files.

- [x] **S04: Safety & Recovery System** `risk:medium` `depends:[S01]`
  > After this: `checkpoint` creates named workspace snapshots, `restore_checkpoint` rolls back, `undo` reverts a single file's last edit, `edit_history` shows the per-file edit stack. Verified by checkpoint → modify files → restore → diff shows no changes.

- [x] **S05: Three-Layer Editing Engine** `risk:medium` `depends:[S02,S04]`
  > After this: `edit_symbol`, `edit_match`, `write`, and `batch` all work with auto-backup, syntax validation, and symbol disambiguation. Verified by editing real code files using all three edit modes and confirming syntax validation catches intentional errors.

- [x] **S06: OpenCode Plugin Bridge** `risk:medium` `depends:[S01,S02,S03,S04,S05]`
  > After this: all M001 commands are available as OpenCode tools — plugin spawns the binary, manages its lifecycle, and registers tools with Zod schemas. Verified by an agent using AFT tools in a real OpenCode session.

- [x] **S07: Binary Distribution Pipeline** `risk:low` `depends:[S06]`
  > After this: `npm install @aft/core` installs the correct platform binary on macOS ARM/Intel, Linux ARM/x64, and Windows x64. `cargo install aft` works as fallback. Verified by CI builds passing on all 5 platforms and npm install test on at least 2 platforms.

## Boundary Map

### S01 → S02

Produces:
- `src/main.rs` — persistent process loop: read JSON line from stdin, dispatch to command handler, write JSON response to stdout
- `src/protocol.rs` — `Request` and `Response` types with serde JSON serialization, `Command` enum, request ID tracking
- `src/language.rs` — `LanguageProvider` trait with `resolve_symbol(file, name) → SymbolMatch`, `list_symbols(file) → Vec<Symbol>` (tree-sitter impl placeholder)
- `src/error.rs` — structured error types: `AftError` enum with `SymbolNotFound`, `AmbiguousSymbol { candidates }`, `ParseError`, `FileNotFound`, `InvalidRequest`
- `src/config.rs` — runtime configuration (validation depth, checkpoint TTL, max depths)
- JSON protocol contract: one JSON object per stdin line, one JSON object per stdout line, `{ "id": "...", "command": "...", ... }` → `{ "id": "...", "ok": bool, ... }`

Consumes: nothing (first slice)

### S02 → S03

Produces:
- `src/parser.rs` — `FileParser` struct: detects language from extension, parses with tree-sitter, caches parsed trees in memory
- `src/symbols.rs` — `Symbol` struct with `name`, `kind` (function/class/method/struct/interface/enum), `range` (start_line, end_line), `signature`, `scope_chain` (qualified name), `exported`, `parent`
- `src/queries/` — per-language tree-sitter query files (.scm) for symbol extraction: `typescript.scm`, `javascript.scm`, `python.scm`, `rust.scm`, `go.scm`
- `LanguageProvider` trait implementation using tree-sitter: `TreeSitterProvider`
- Language detection: file extension → tree-sitter grammar mapping for .ts, .tsx, .js, .jsx, .py, .rs, .go

Consumes from S01:
- `protocol.rs` → `Request`/`Response` types for command dispatch
- `language.rs` → `LanguageProvider` trait (implements it)
- `error.rs` → `AftError` variants for parse failures

### S03 → S05

Produces:
- `outline` command handler: accepts `{ "command": "outline", "file": "path" }`, returns structured symbol list with kinds, ranges, signatures, member nesting, export status
- `zoom` command handler: accepts `{ "command": "zoom", "file": "path", "symbol": "name" }`, returns symbol body with `context_before`, `context_after`, `content`, `annotations.calls_out`, `annotations.called_by`
- `src/commands/outline.rs`, `src/commands/zoom.rs`

Consumes from S02:
- `parser.rs` → `FileParser` for parsing files
- `symbols.rs` → `Symbol` type for structured results
- `queries/` → language-specific symbol extraction

### S04 → S05

Produces:
- `src/backup.rs` — `BackupStore`: `snapshot(file_path) → backup_id`, `restore(file_path) → Result`, `history(file_path) → Vec<EditRecord>`, in-memory undo stack with periodic flush to `.aft/backups/`
- `src/checkpoint.rs` — `CheckpointStore`: `create(name) → checkpoint_id`, `restore(name) → Result`, `list() → Vec<Checkpoint>`, `cleanup(ttl_hours)`, stored in `.aft/checkpoints/`
- `undo`, `edit_history`, `checkpoint`, `restore_checkpoint`, `list_checkpoints` command handlers

Consumes from S01:
- `protocol.rs` → `Request`/`Response` types
- `config.rs` → checkpoint TTL configuration

### S05 → S06

Produces:
- `edit_symbol` command: accepts symbol name + operation (replace/delete/insert_before/insert_after) + content, resolves symbol via tree-sitter, applies edit, auto-backups, validates syntax, returns new range + `syntax_valid` + `backup_id`
- `edit_match` command: accepts match string or from/to range markers + replacement, handles ambiguity (multiple matches → return candidates with context)
- `write` command: accepts file path + full content, creates dirs if needed, auto-backups existing file
- `batch` command: accepts array of edits for one file, sorts bottom-to-top, applies atomically (all or rollback)
- `src/edit.rs` — edit engine: symbol resolution → range calculation → content replacement → backup → syntax validation
- `src/commands/edit_symbol.rs`, `src/commands/edit_match.rs`, `src/commands/write.rs`, `src/commands/batch.rs`

Consumes from S02:
- `parser.rs` → re-parse after edit for syntax validation
- `symbols.rs` → symbol resolution for edit_symbol
- `language.rs` → `LanguageProvider.resolve_symbol()` for symbol lookup

Consumes from S04:
- `backup.rs` → `BackupStore.snapshot()` before every mutation

### S06 → S07

Produces:
- `opencode-plugin-aft/src/index.ts` — plugin entry: exports `Plugin` that returns tool registrations
- `opencode-plugin-aft/src/bridge.ts` — `BinaryBridge` class: spawns aft binary as persistent child process, sends JSON commands via stdin, reads JSON responses from stdout, handles crash detection and auto-restart, health check (ping command)
- `opencode-plugin-aft/src/resolver.ts` — finds binary: check npm platform package → check PATH → check cargo install location → error with install instructions
- `opencode-plugin-aft/src/tools/` — tool registration files with Zod schemas for each command: outline, zoom, edit_symbol, edit_match, write, batch, undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints
- `opencode-plugin-aft/package.json` — plugin package with `@opencode-ai/plugin` dependency

Consumes from S01–S05:
- JSON protocol contract (command shapes, response shapes, error formats)
- All command specifications for Zod schema definitions

### S07 (terminal slice)

Produces:
- `.github/workflows/release.yml` — CI pipeline: cross-compile Rust binary for 5 platforms (darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64)
- `npm/` directory with platform package scaffolds: `@aft/darwin-arm64`, `@aft/darwin-x64`, `@aft/linux-arm64`, `@aft/linux-x64`, `@aft/win32-x64`
- `@aft/core` package: plugin code + binary resolver + optionalDependencies on all platform packages
- `Cargo.toml` metadata for `cargo install aft` fallback
- Release automation: tag → build → publish npm packages

Consumes from S06:
- Plugin code (packaged into @aft/core)
- Binary resolver (knows where to find platform binaries)
