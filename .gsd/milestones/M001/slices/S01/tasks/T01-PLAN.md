---
estimated_steps: 7
estimated_files: 7
---

# T01: Implement persistent binary with protocol types and command dispatch

**Slice:** S01 — Binary Scaffold & Persistent Protocol
**Milestone:** M001

## Description

Build the complete `aft` binary from scratch — Cargo project setup, all core source modules, the NDJSON protocol contract, three bootstrap commands (ping, version, echo), structured error handling, the `LanguageProvider` trait stub for S02, and runtime config. This is the foundation every subsequent slice builds on.

## Steps

1. Initialize Cargo project with `cargo init --name aft`. Add dependencies: `serde` (derive), `serde_json`. Set edition 2021, define binary name `aft`.
2. Create `src/error.rs` — `AftError` enum with variants: `SymbolNotFound { name, file }`, `AmbiguousSymbol { name, candidates: Vec<String> }`, `ParseError { message }`, `FileNotFound { path }`, `InvalidRequest { message }`. Implement `Display` and `std::error::Error`. Add a method to produce error response JSON (`code` string + `message` string).
3. Create `src/protocol.rs` — `RawRequest` struct with `id: String`, `command: String`, `params: serde_json::Value` (using `#[serde(flatten)]`). `Response` struct with `id: String`, `ok: bool`, and flattened data/error fields. Per-command param structs: `EchoParams { message: String }`. Response helpers for success and error. Include optional `lsp_hints: Option<serde_json::Value>` in `RawRequest` for R031 forward compatibility.
4. Create `src/config.rs` — `Config` struct with `project_root: Option<PathBuf>`, `validation_depth: u32` (default 1), `checkpoint_ttl_hours: u32` (default 24), `max_symbol_depth: u32` (default 10). `Default` impl with sensible values.
5. Create `src/language.rs` — `LanguageProvider` trait with two methods: `resolve_symbol(file: &Path, name: &str) -> Result<Vec<SymbolMatch>, AftError>` and `list_symbols(file: &Path) -> Result<Vec<Symbol>, AftError>`. Define minimal `Symbol` and `SymbolMatch` structs (name, kind, range fields) that S02 will flesh out. Add a `StubProvider` that returns `AftError::InvalidRequest` for any call (placeholder).
6. Create `src/main.rs` — persistent process loop using `BufReader::lines()` on stdin, `BufWriter` on stdout. For each line: skip empty lines, trim whitespace, attempt two-stage parse (JSON parse → extract id+command → dispatch). On parse failure: write error response with sentinel id `_parse_error`. On success: match command string to handler (ping → `{"command":"pong"}`, version → `{"version":"0.1.0"}`, echo → return params). Write response as single JSON line + `\n` + explicit `flush()`. On stdin EOF: `eprintln!("[aft] stdin closed, shutting down")` and exit 0. Startup banner to stderr: `eprintln!("[aft] started, pid {}", std::process::id())`.
7. Create `src/lib.rs` — re-export `protocol`, `error`, `config`, `language` modules (makes them available for integration tests). Add unit tests: protocol round-trip serialization, error Display formatting, config Default values, RawRequest deserialization with unknown fields preserved.

## Must-Haves

- [ ] Two-stage request parsing (RawRequest envelope → command dispatch)
- [ ] ping, version, echo command handlers
- [ ] Structured error responses with `code` and `message`
- [ ] `BufWriter` flush after every response write
- [ ] All diagnostics to stderr, stdout is JSON-only
- [ ] `LanguageProvider` trait with `resolve_symbol` and `list_symbols`
- [ ] `AftError` enum with all five variants from boundary map
- [ ] `Config` struct with project root and runtime settings
- [ ] `lsp_hints` optional field in request type
- [ ] Unit tests for serialization, error formatting, config defaults

## Verification

- `cargo build` succeeds with zero warnings (`cargo build 2>&1 | grep -c warning` returns 0)
- `cargo test --lib` passes all unit tests
- `echo '{"id":"1","command":"ping"}' | cargo run` outputs `{"id":"1","ok":true,"command":"pong"}`
- `echo 'not json' | cargo run` outputs error response with `_parse_error` id, then exits cleanly
- `echo '{"id":"2","command":"echo","message":"hello"}' | cargo run` echoes the message back

## Observability Impact

- Signals added: `[aft] started, pid {pid}` and `[aft] stdin closed, shutting down` on stderr
- How a future agent inspects this: read stderr output of the process, send `ping` command for health, `version` for identification
- Failure state exposed: error responses include `code` (e.g., `"unknown_command"`, `"parse_error"`) and `message` with details

## Inputs

- S01-RESEARCH.md — two-stage parse pattern, serde flatten approach, BufWriter flush requirement, tree-sitter crate versions
- M001-ROADMAP.md boundary map — file structure and type contracts S02 will consume
- DECISIONS.md — D001 (persistent process), D009 (NDJSON protocol)

## Expected Output

- `Cargo.toml` — project manifest with serde/serde_json dependencies
- `src/main.rs` — persistent process loop with command dispatch
- `src/lib.rs` — module declarations and re-exports, unit tests
- `src/protocol.rs` — request/response types with serde serialization
- `src/error.rs` — `AftError` enum with Display and error response generation
- `src/config.rs` — `Config` struct with defaults
- `src/language.rs` — `LanguageProvider` trait and stub implementation
