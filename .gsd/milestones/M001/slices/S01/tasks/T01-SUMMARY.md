---
id: T01
parent: S01
milestone: M001
provides:
  - aft binary with persistent process loop
  - NDJSON protocol types (RawRequest, Response)
  - three bootstrap commands (ping, version, echo)
  - AftError enum with five variants
  - LanguageProvider trait with StubProvider
  - Config struct with runtime defaults
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
  - main.rs imports from lib crate (use aft::) rather than re-declaring modules, avoiding dead_code warnings
  - integration test dir uses main.rs entry point (Cargo convention for multi-file integration tests)
patterns_established:
  - two-stage request parsing — RawRequest envelope then command string dispatch
  - Response::success/error constructors with serde flatten for clean JSON output
  - BufWriter with explicit flush after every response write
  - all diagnostics to stderr with [aft] prefix, stdout is JSON-only
observability_surfaces:
  - "[aft] started, pid {pid}" on stderr at startup
  - "[aft] stdin closed, shutting down" on stderr at shutdown
  - "[aft] parse error: ... — input: ..." on stderr for malformed JSON
  - "[aft] unknown command: ..." on stderr for unrecognized commands
  - ping command as health check, version command for identification
  - structured error responses with code and message fields
duration: ~25min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Implement persistent binary with protocol types and command dispatch

**Built the complete aft binary — Cargo project, NDJSON protocol, three commands, structured errors, LanguageProvider trait stub, config, and 19 passing tests.**

## What Happened

Initialized a Cargo project with serde/serde_json dependencies. Created all six source modules:

- `error.rs`: AftError enum with SymbolNotFound, AmbiguousSymbol, ParseError, FileNotFound, InvalidRequest. Display impl, code() method, and to_error_json() helper.
- `protocol.rs`: RawRequest with serde flatten for params capture, optional lsp_hints field. Response with flatten data. EchoParams struct. Success/error constructors.
- `config.rs`: Config struct with project_root, validation_depth (1), checkpoint_ttl_hours (24), max_symbol_depth (10).
- `language.rs`: LanguageProvider trait with resolve_symbol and list_symbols. Symbol, SymbolMatch, Range structs. StubProvider that returns InvalidRequest.
- `main.rs`: Persistent process loop — BufReader on stdin, BufWriter on stdout, two-stage parse (JSON → RawRequest → command dispatch), three handlers (ping/version/echo), explicit flush per response, stderr diagnostics, clean exit on EOF.
- `lib.rs`: Module re-exports and 13 unit tests covering protocol round-trips, error Display, error JSON, config defaults, lsp_hints, and unknown field preservation.

Also scaffolded integration tests (6 basic tests) for T02 to extend with the 100+ command suite.

## Verification

- `cargo build` — 0 warnings
- `cargo test --lib` — 13 unit tests pass
- `cargo test --test integration` — 6 integration tests pass (ping, version, echo, malformed JSON, unknown command, clean shutdown)
- Manual: `echo '{"id":"1","command":"ping"}' | cargo run` → `{"id":"1","ok":true,"command":"pong"}`
- Manual: `echo 'not json' | cargo run` → error response with `_parse_error` id
- Manual: `echo '{"id":"2","command":"echo","message":"hello"}' | cargo run` → `{"id":"2","ok":true,"message":"hello"}`
- Stderr shows startup/shutdown banners, process exits with code 0

### Slice-level verification status

- ✅ `cargo build` succeeds with no warnings
- ✅ `cargo test` passes all unit tests
- ⬜ 100+ sequential commands test — T02 (scaffolding created)
- ✅ Malformed JSON recovery — passes
- ✅ Clean shutdown on stdin EOF — passes

## Diagnostics

- Send `{"id":"h","command":"ping"}` on stdin → expect `{"id":"h","ok":true,"command":"pong"}` within 1ms
- Send `{"id":"v","command":"version"}` → response includes `"version":"0.1.0"`
- Stderr lines prefixed with `[aft]` for startup, shutdown, parse errors, unknown commands
- Error responses always have `code` (string) and `message` (string) fields

## Deviations

- Created integration test scaffolding with 6 basic tests (plan said T02 creates these, but slice plan says first task creates test files that initially fail — they pass instead since the binary works). T02 will extend with the 100+ command suite.
- Used `tests/integration/main.rs` instead of `mod.rs` per Cargo convention for multi-file integration test directories.

## Known Issues

None.

## Files Created/Modified

- `Cargo.toml` — project manifest with serde/serde_json deps
- `src/main.rs` — persistent process loop with command dispatch
- `src/lib.rs` — module re-exports and 13 unit tests
- `src/protocol.rs` — RawRequest, Response, EchoParams types
- `src/error.rs` — AftError enum with five variants
- `src/config.rs` — Config struct with runtime defaults
- `src/language.rs` — LanguageProvider trait and StubProvider
- `tests/integration/main.rs` — integration test entry point
- `tests/integration/protocol_test.rs` — 6 basic integration tests
