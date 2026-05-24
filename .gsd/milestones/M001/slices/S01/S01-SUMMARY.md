---
id: S01
parent: M001
milestone: M001
provides:
  - persistent Rust binary with NDJSON stdin/stdout protocol
  - two-stage request parsing (envelope → command dispatch)
  - three bootstrap commands (ping, version, echo)
  - AftError enum with five structured error variants
  - LanguageProvider trait with StubProvider placeholder
  - Config struct with runtime defaults
  - AftProcess integration test harness
requires: []
affects:
  - S02
  - S03
  - S04
  - S05
  - S06
key_files:
  - Cargo.toml
  - src/main.rs
  - src/lib.rs
  - src/protocol.rs
  - src/error.rs
  - src/config.rs
  - src/language.rs
  - tests/integration/main.rs
  - tests/integration/protocol_test.rs
key_decisions:
  - two-stage request parsing — RawRequest envelope deserialized first, then command string dispatched to per-command handlers
  - main.rs imports from lib crate (use aft::) to avoid dead_code warnings on shared types
  - integration tests use persistent BufReader via AftProcess struct — per-call BufReader loses buffered data during sequential reads
patterns_established:
  - RawRequest envelope with serde flatten for arbitrary params capture
  - Response::success/error constructors with serde flatten for clean JSON output
  - BufWriter with explicit flush after every response write
  - all diagnostics to stderr with [aft] prefix, stdout is JSON-only
  - AftProcess spawn/send/shutdown pattern for integration tests
observability_surfaces:
  - "[aft] started, pid {pid}" on stderr at startup
  - "[aft] stdin closed, shutting down" on stderr at shutdown
  - "[aft] parse error: ... — input: ..." on stderr for malformed JSON
  - "[aft] unknown command: ..." on stderr for unrecognized commands
  - ping command as health check, version command for binary identification
  - structured error responses with code and message fields
drill_down_paths:
  - .gsd/milestones/M001/slices/S01/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S01/tasks/T02-SUMMARY.md
duration: ~35min
verification_result: passed
completed_at: 2026-03-14
---

# S01: Binary Scaffold & Persistent Protocol

**Persistent Rust binary accepting newline-delimited JSON commands on stdin, dispatching to command handlers, and writing JSON responses to stdout — proven reliable across 120 sequential commands, 8 malformed recovery scenarios, and clean shutdown.**

## What Happened

T01 built the complete binary: Cargo project with serde/serde_json, six source modules (main, protocol, error, config, language, lib), and three bootstrap commands (ping, version, echo). The protocol uses two-stage parsing — first deserialize the JSON envelope (id + command + flattened params), then dispatch on the command string to per-command handlers. All diagnostic output goes to stderr with `[aft]` prefix; stdout is exclusively JSON responses. The LanguageProvider trait defines `resolve_symbol` and `list_symbols` signatures with a StubProvider returning InvalidRequest — ready for S02's tree-sitter implementation.

T02 rewrote the integration test scaffold with a persistent `AftProcess` helper struct. The T01 scaffold used per-call BufReader which silently lost buffered data during sequential reads — the rewrite holds a single BufReader over the child process's stdout for the test lifetime. Four tests prove the reliability contract: 120 sequential commands with ID matching, 8 malformed JSON recovery scenarios, unknown command error handling, and clean shutdown with stderr lifecycle banner assertions.

## Verification

- `cargo build` — 0 warnings
- `cargo test` — 17 tests pass (13 unit + 4 integration)
- **Sequential throughput:** 120 commands (ping/version/echo cycling) with correct ID matching and payload validation
- **Malformed recovery:** garbage text, empty lines, whitespace-only, truncated JSON, missing required fields — all recover, next valid command succeeds
- **Unknown commands:** structured error with `code: "unknown_command"` and command name in message
- **Clean shutdown:** stdin EOF → exit code 0, stderr contains startup and shutdown banners
- **Manual verification:** `echo '{"id":"1","command":"ping"}' | cargo run` → correct JSON response, stderr shows lifecycle banners

## Requirements Advanced

- R001 (Persistent binary architecture) — fully proven: binary runs as persistent process, receives JSON on stdin, responds on stdout, stays alive between commands, handles graceful shutdown
- R032 (Structured JSON I/O) — fully proven: all communication is JSON over stdin/stdout, no shell arguments
- R031 (LSP-aware architecture) — optional `lsp_hints` field present in RawRequest protocol type, provider interface defined

## Requirements Validated

- R001 — persistent process loop proven by 120 sequential commands without restart, clean shutdown on EOF
- R032 — JSON I/O proven by all commands flowing through serde JSON, no shell escaping in any path

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- T01 created 6 integration tests as scaffold (plan said T02 would create all tests). T02 replaced them entirely with 4 comprehensive tests using the persistent AftProcess pattern.
- Used `tests/integration/main.rs` instead of `mod.rs` — follows Cargo convention for multi-file integration test directories.

## Known Limitations

- Only three commands (ping, version, echo) — no real functionality yet. S02+ adds parsing and real operations.
- LanguageProvider trait has only a stub implementation — returns InvalidRequest for all calls.
- No file I/O, no tree-sitter, no checkpoint system — this is purely the protocol and process scaffold.

## Follow-ups

- none — all planned work complete, no deferred items discovered

## Files Created/Modified

- `Cargo.toml` — project manifest with serde/serde_json dependencies
- `src/main.rs` — persistent process loop with two-stage request parsing and command dispatch
- `src/lib.rs` — module re-exports and 13 unit tests
- `src/protocol.rs` — RawRequest, Response, EchoParams types with serde flatten
- `src/error.rs` — AftError enum with five variants, Display impl, code() and to_error_json()
- `src/config.rs` — Config struct with project_root, validation_depth, checkpoint_ttl_hours, max_symbol_depth
- `src/language.rs` — LanguageProvider trait, Symbol/SymbolMatch/Range types, StubProvider
- `tests/integration/main.rs` — integration test entry point
- `tests/integration/protocol_test.rs` — 4 comprehensive integration tests with AftProcess helper

## Forward Intelligence

### What the next slice should know
- The command dispatch in `main.rs` is a simple `match` on the command string — add new commands by adding match arms and importing the handler
- `RawRequest.params` is a `serde_json::Map<String, Value>` captured via `#[serde(flatten)]` — extract per-command params from this map
- `Response::success(id, data)` and `Response::error(id, code, message)` are the constructors for all responses
- The `LanguageProvider` trait in `language.rs` defines `resolve_symbol` and `list_symbols` — S02 implements these with tree-sitter

### What's fragile
- The `match` dispatch in `main.rs` will grow linearly with commands — may want a registry pattern by S05/S06 when there are 10+ commands, but it's fine for now
- `StubProvider` returns `InvalidRequest` for everything — any code path that actually calls it will get an error, which is correct until S02 wires in the real implementation

### Authoritative diagnostics
- `cargo test --test integration -- --nocapture` shows `[test]` prefixed output with command counts and scenario labels
- stderr of the binary shows `[aft]` prefixed lifecycle events — startup, shutdown, parse errors, unknown commands
- Integration test assertion messages include response JSON and index for pinpointing sequential failures

### What assumptions changed
- Per-call BufReader for integration tests doesn't work — buffered data is lost between calls. The AftProcess pattern with a persistent BufReader is required for any test sending multiple commands.
