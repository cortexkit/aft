# S01: Binary Scaffold & Persistent Protocol

**Goal:** A Rust binary that runs as a persistent process, accepting newline-delimited JSON commands on stdin and writing JSON responses to stdout, staying alive between commands.
**Demo:** Integration test spawns the binary, sends 100+ sequential commands (ping, version, echo, malformed JSON), verifies correct responses, and confirms clean shutdown on stdin EOF.

## Must-Haves

- Persistent process loop: read JSON line from stdin → dispatch → write JSON response to stdout → repeat
- Two-stage request parsing: deserialize `{ id, command, ...params }` envelope first, then dispatch on `command` string
- Three commands: `ping` (returns pong), `version` (returns version string), `echo` (returns the params back)
- Structured error responses for unknown commands, malformed JSON, and missing required fields
- `LanguageProvider` trait with `resolve_symbol` and `list_symbols` signatures (stub — no implementation)
- `AftError` enum with variants: `SymbolNotFound`, `AmbiguousSymbol`, `ParseError`, `FileNotFound`, `InvalidRequest`
- Runtime config struct with project root, validation depth, checkpoint TTL
- All diagnostic output to stderr — stdout is JSON-only
- `BufWriter` flush after every response
- Clean exit on stdin EOF
- Recovery from malformed JSON without crashing (next command processes normally)
- Request ID echoed in every response (sentinel `_parse_error` when request can't be parsed)
- Optional `lsp_hints` field in protocol types for LSP enrichment path (R031)

## Proof Level

- This slice proves: contract + operational
- Real runtime required: yes (binary must actually run as a persistent process)
- Human/UAT required: no

## Verification

- `cargo build` succeeds with no warnings
- `cargo test` passes all unit tests (protocol serialization, error formatting, config defaults)
- `tests/integration/protocol_test.rs` — spawns the binary, sends 100+ sequential commands, asserts correct JSON responses
- `tests/integration/protocol_test.rs` — sends malformed JSON, verifies error response, then sends valid command and verifies it succeeds (recovery)
- `tests/integration/protocol_test.rs` — closes stdin, verifies process exits with code 0 (clean shutdown)

## Observability / Diagnostics

- Runtime signals: `eprintln!` with `[aft]` prefix for startup, shutdown, and error events on stderr
- Inspection surfaces: `ping` command as health check; `version` command for binary identification
- Failure visibility: structured error responses with `code` and `message` fields; parse errors include the raw input that failed

## Integration Closure

- Upstream surfaces consumed: none (first slice)
- New wiring introduced in this slice: persistent process loop, JSON protocol contract (consumed by every subsequent slice)
- What remains before the milestone is truly usable end-to-end: S02 (parsing), S03 (reading), S04 (safety), S05 (editing), S06 (plugin), S07 (distribution)

## Tasks

- [x] **T01: Implement persistent binary with protocol types and command dispatch** `est:2h`
  - Why: builds the complete binary — all source modules, protocol contract, command handlers, error types, config, and language trait stub
  - Files: `Cargo.toml`, `src/main.rs`, `src/protocol.rs`, `src/error.rs`, `src/config.rs`, `src/language.rs`, `src/lib.rs`
  - Do: init Cargo project with serde/serde_json deps. Implement two-stage request parsing (RawRequest → command string match → per-command handler). Three commands: ping, version, echo. Structured error responses. BufWriter with explicit flush. eprintln diagnostics to stderr. LanguageProvider trait with two method signatures. AftError enum. Config struct with defaults. Unit tests for serialization round-trips and error formatting.
  - Verify: `cargo build` succeeds, `cargo test` passes, manual `echo '{"id":"1","command":"ping"}' | cargo run` returns `{"id":"1","ok":true,"command":"pong"}`
  - Done when: binary compiles, unit tests pass, manual stdin/stdout test works

- [x] **T02: Integration tests proving process reliability contract** `est:1h`
  - Why: proves the operational requirements — 100+ sequential commands, malformed JSON recovery, and clean shutdown. This is the slice's demo.
  - Files: `tests/integration/protocol_test.rs`, `tests/integration/mod.rs`
  - Do: write integration tests that spawn the built binary as a child process. Test 1: send 100+ sequential ping/echo commands, assert each response has correct id and data. Test 2: send malformed JSON, assert error response, then send valid command and assert success (recovery). Test 3: send commands then close stdin, assert process exits cleanly. Test 4: send unknown command, assert structured error with "unknown command" message.
  - Verify: `cargo test --test integration` passes all tests
  - Done when: all integration tests pass, demonstrating 100+ sequential commands, malformed recovery, unknown command handling, and clean shutdown

## Files Likely Touched

- `Cargo.toml`
- `src/main.rs`
- `src/lib.rs`
- `src/protocol.rs`
- `src/error.rs`
- `src/config.rs`
- `src/language.rs`
- `tests/integration/protocol_test.rs`
- `tests/integration/mod.rs`
