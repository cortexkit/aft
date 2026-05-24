# S01: Binary Scaffold & Persistent Protocol — Research

**Date:** 2026-03-14

## Summary

S01 is a greenfield Rust binary — no existing code, no prior art in this repo. The slice produces the persistent process loop, the JSON protocol types, the `LanguageProvider` trait (placeholder for S02), structured error types, and runtime config. Three bootstrap commands: `ping`, `version`, `echo`.

The core technical pattern — newline-delimited JSON over stdin/stdout with BufReader/BufWriter — is straightforward and well-proven (LSP, tree-sitter CLI, esbuild all use variants). A prototype confirmed: two-stage parse (extract `id` + `command` first, then dispatch) gives clean error handling for unknown commands and malformed JSON without crashing the process. `#[serde(flatten)]` on a `params: serde_json::Value` field cleanly separates protocol envelope from command-specific data.

All 6 tree-sitter grammar crates compile and parse correctly with tree-sitter 0.26.6 — no version compatibility issues. Root node kinds: `program` (TS/TSX/JS), `module` (Python), `source_file` (Go/Rust). TypeScript crate exposes `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX` as separate constants.

## Recommendation

**Two-stage request parsing with manual dispatch.** Don't use serde's internally tagged enum for the `Command` dispatch — it gives opaque errors for unknown commands. Instead:

1. Deserialize to `RawRequest { id, command, params: Value }`
2. Match on `command` string → per-command handler
3. Each handler deserializes its specific params from the `Value`

This gives precise error messages ("unknown command: foo") and lets each command define its own param types without polluting a single enum. The boundary map calls for a `Command` enum — implement it as a string-dispatched enum with individual param structs rather than a serde-tagged union.

For responses, use `#[serde(flatten)]` to merge the envelope (`id`, `ok`) with command-specific response data. Errors follow a consistent `{ code, message, details? }` structure.

The `LanguageProvider` trait should be minimal — just the signatures S02 needs to implement. Don't over-design it. Two methods: `resolve_symbol(file, name) → Result<Vec<SymbolMatch>>` and `list_symbols(file) → Result<Vec<Symbol>>`. S02 will flesh out the types.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|-------------------|------------|
| JSON serialization | `serde` 1.x + `serde_json` 1.x | Industry standard, zero-cost abstractions, derive macros |
| Unique request IDs (testing) | `uuid` crate or let the caller provide IDs | Caller-provided IDs are simpler — the binary doesn't generate IDs, it echoes them |
| Line-buffered I/O | `std::io::BufReader` / `BufWriter` | Standard library, no dependency needed |
| Process signal handling | `ctrlc` crate or raw `libc` signals | For graceful shutdown on SIGTERM/SIGINT — `ctrlc` is simpler |

## Existing Code and Patterns

- No existing code — greenfield repository. Only `.gsd/` and `.gitignore` exist.
- OpenCode plugin API (`@opencode-ai/plugin`): `tool()` helper with Zod schemas, `execute(args, context)` where `context` provides `directory` and `worktree`. S06 will consume this — S01 only needs to be aware of it for protocol design.
- The esbuild/turbo pattern for npm binary distribution (S07) — S01 should produce a binary name that works with this (`aft` binary).

## Constraints

- **stdout is JSON-only** — all debug/diagnostic output must go to stderr. A single non-JSON byte on stdout corrupts the protocol for the consuming plugin.
- **BufWriter must flush after every response** — without explicit flush, responses buffer and the plugin hangs waiting.
- **One JSON object per line** (D009) — no pretty-printing on stdout. `serde_json::to_writer` + `\n` + flush.
- **Caller-provided request IDs** — the binary echoes back the `id` from the request. If the request can't be parsed, use a sentinel ID (e.g., `"_parse_error"`).
- **Rust 1.77+ required** (tree-sitter crate minimum). Current toolchain is 1.93 stable — no issue.
- **Process must exit cleanly on stdin EOF** — this is the normal shutdown path (plugin kills the child process or closes stdin).

## Common Pitfalls

- **Logging to stdout** — Any `println!` or `dbg!` corrupts the protocol stream. Use `eprintln!` for all diagnostics, and consider a structured logging approach to stderr from the start (even just `eprintln!` with a prefix).
- **Forgetting to flush BufWriter** — The most common bug in stdin/stdout protocol implementations. Every response write must end with an explicit `flush()`.
- **Panic in command handler kills the process** — Use `std::panic::catch_unwind` around command dispatch, or structure handlers so they return `Result` and never panic. The latter is cleaner for S01's simple commands.
- **Windows line endings** — `BufReader::read_line` includes the trailing `\n` (or `\r\n` on Windows). Must trim before JSON parsing. `line.trim()` handles both.
- **Large file content in JSON** — S01 doesn't handle file content, but the protocol must not assume line length limits. `read_line` grows the buffer as needed — this is fine, but S05 should be aware that editing a 10MB file means a 10MB+ JSON line.
- **serde flatten + unknown fields** — `#[serde(flatten)]` with `Value` captures all extra fields. This is the desired behavior for forward compatibility (new fields don't break old binary versions).

## Open Risks

- **Graceful shutdown race condition** — If the plugin sends a command and immediately closes stdin, the binary might not finish writing the response before stdout closes. Mitigation: flush synchronously before checking for next input. Low risk for S01.
- **Memory growth over long sessions** — S01 is stateless (no caching), but S02+ will add parse tree caches and S04 adds backup/checkpoint stores. The architecture should be aware that the persistent process will accumulate state. S01 doesn't need to solve this, but the main loop should be structured so state is held in an explicit `AppState` struct, not globals.
- **Concurrent requests** — The protocol is synchronous (one request, one response, in order). The plugin must not send a second request before receiving the first response. S01 should document this constraint. If concurrency is ever needed, it's a protocol version bump.

## Key Technical Findings

### Tree-sitter crate compatibility (verified)

| Crate | Version | Compatible with tree-sitter 0.26.6 | Root node kind |
|-------|---------|-------------------------------------|----------------|
| `tree-sitter-typescript` | 0.23.2 | ✅ | `program` |
| `tree-sitter-javascript` | 0.25.0 | ✅ | `program` |
| `tree-sitter-python` | 0.25.0 | ✅ | `module` |
| `tree-sitter-go` | 0.25.0 | ✅ | `source_file` |
| `tree-sitter-rust` | 0.24.0 | ✅ | `source_file` |

TypeScript crate exposes two language constants: `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX`. All others expose `LANGUAGE`.

### Protocol prototype results (verified)

A prototype confirmed:
- Two-stage parse (`RawRequest` with `#[serde(flatten)]`) works cleanly
- Malformed JSON returns error response without crashing — next command processes normally
- `BufWriter` + explicit `flush()` delivers responses immediately
- stdin EOF causes clean process exit (the `lines()` iterator ends)
- Empty lines are safely skipped

### Serde enum representations

- **Internally tagged** (`#[serde(tag = "command")]`): works but gives opaque error for unknown variants
- **Two-stage parse** (manual string match on `command` field): gives precise "unknown command" errors, allows per-command param structs, more extensible
- **Recommendation**: two-stage parse for requests, `#[serde(flatten)]` for responses

### File structure (from boundary map)

S01 produces these files consumed by later slices:
- `src/main.rs` — process loop, stdin/stdout I/O
- `src/protocol.rs` — `RawRequest`, `Response`, command-specific param/result types
- `src/language.rs` — `LanguageProvider` trait (stub — S02 implements it)
- `src/error.rs` — `AftError` enum with variants matching the roadmap
- `src/config.rs` — runtime config struct
- `Cargo.toml` — workspace setup with dependencies

### Requirements mapped to this slice

| Requirement | Role | What S01 must deliver |
|-------------|------|----------------------|
| R001 — Persistent binary architecture | Primary | Process loop, stays alive, handles sequential commands, recovers from malformed input |
| R031 — LSP-aware architecture | Primary | `LanguageProvider` trait with provider interface; optional `lsp_hints` in protocol types |
| R032 — Structured JSON I/O | Primary | NDJSON protocol, request/response types, no shell arguments |
| R027 — Worktree-aware scoping | Supporting | Config struct with project root awareness (actual scoping logic in S02+) |

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | `apollographql/skills@rust-best-practices` (2.3K installs) | Available — generic Rust patterns, may help with idiomatic error handling |
| Rust | `jeffallan/claude-skills@rust-engineer` (1.1K installs) | Available — general Rust development |
| Tree-sitter | `plurigrid/asi@tree-sitter` (7 installs) | Available — too niche/low adoption to recommend |

None are essential for S01. The Rust skills are generic best-practices — useful if you want Rust idiom guidance but not required for this protocol work.

## Sources

- Tree-sitter crate versions confirmed compatible via `cargo build` test with all 6 grammars (local verification)
- Protocol pattern validated via stdin/stdout prototype — malformed JSON recovery, BufWriter flush, stdin EOF shutdown (local verification)
- Serde enum representations — internally tagged vs adjacently tagged vs two-stage parse (source: [serde.rs/enum-representations](https://serde.rs/enum-representations))
- OpenCode plugin API — `tool()` helper, `execute(args, context)` with `directory`/`worktree` (source: [opencode.ai/docs/plugins](https://opencode.ai/docs/plugins/index))
- TypeScript grammar exposes `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX` separately (source: `tree-sitter-typescript` 0.23.2 crate, verified in build test)
