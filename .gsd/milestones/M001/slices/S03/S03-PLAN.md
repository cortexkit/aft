# S03: Structural Reading

**Goal:** The aft binary handles `outline` and `zoom` commands — outline returns a file's nested symbol structure, zoom returns a single symbol's body with surrounding context and file-scoped caller/callee annotations.

**Demo:** Send `outline` for a multi-symbol TS file through the binary protocol → get nested symbol tree with kinds, ranges, signatures, export status. Send `zoom` for a specific function → get its body, context lines, calls_out list, called_by list.

## Must-Haves

- `outline` command returns nested symbol tree (methods under classes, not duplicated at top level)
- `zoom` command returns symbol body with configurable context lines (default 3)
- `zoom` annotates outbound calls (calls_out) and inbound callers (called_by) within the same file
- Ambiguous symbol names in zoom produce `AmbiguousSymbol` error with candidates (R011 support)
- Both commands wired into main.rs dispatch via `src/commands/` module pattern
- `TreeSitterProvider` (`_provider`) becomes the active provider passed to command handlers
- All 7 SymbolKind variants handled in outline nesting and zoom

## Proof Level

- This slice proves: contract (command input/output shapes via integration tests against the binary)
- Real runtime required: yes (spawns binary, sends JSON commands, verifies responses)
- Human/UAT required: no

## Verification

- `cargo test --lib -- commands::outline::tests` — outline unit tests: flat-to-tree nesting, export preservation, all symbol kinds
- `cargo test --lib -- commands::zoom::tests` — zoom unit tests: body extraction, context lines, call annotation extraction
- `cargo test --test integration` — integration tests sending outline/zoom commands through binary stdin/stdout protocol
- `cargo build` — 0 errors, 0 warnings

## Observability / Diagnostics

- Runtime signals: `AftError` JSON responses with `code` field — `"symbol_not_found"` for missing zoom targets, `"ambiguous_symbol"` for multiple matches, `"file_not_found"` for missing files, `"invalid_request"` for missing params
- Inspection surfaces: stderr `[aft]` prefix logs for parse failures and unknown commands
- Failure visibility: error responses include structured code + message for every failure path
- Redaction constraints: none

## Integration Closure

- Upstream surfaces consumed: `TreeSitterProvider.list_symbols()` and `resolve_symbol()` from S02 parser.rs, `Response::success()`/`Response::error()` from S01 protocol.rs, `AftError` variants from error.rs
- New wiring introduced in this slice: `src/commands/` module directory, dispatch arms in main.rs for "outline" and "zoom", `_provider` becomes active dependency
- What remains before the milestone is truly usable end-to-end: S04 (safety/recovery), S05 (editing engine), S06 (plugin bridge), S07 (distribution)

## Tasks

- [x] **T01: Outline command with dispatch wiring** `est:1h`
  - Why: Establishes the `src/commands/` module pattern and delivers the outline command — the simpler of the two commands, which validates the architecture before zoom adds complexity.
  - Files: `src/commands/mod.rs`, `src/commands/outline.rs`, `src/main.rs`, `src/lib.rs`, `tests/integration/commands_test.rs`
  - Do: Create commands module. Build outline handler that transforms flat symbol list into nested tree (children under parents via `parent` field). Define `OutlineParams` and `OutlineEntry` serializable types. Wire into main.rs dispatch — rename `_provider` to `provider`, pass to dispatch function. Add integration tests using AftProcess pattern against existing fixture files.
  - Verify: `cargo test --lib -- commands::outline::tests` and `cargo test --test integration -- test_outline` all pass, `cargo build` with 0 warnings
  - Done when: outline command returns correct nested symbol trees for TS, Python, and Rust fixture files through the binary protocol

- [x] **T02: Zoom command with caller/callee annotations** `est:1.5h`
  - Why: Delivers the zoom command with symbol body extraction and file-scoped call annotations — the higher-complexity command that completes S03.
  - Files: `src/commands/zoom.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/fixtures/calls.ts`, `tests/integration/commands_test.rs`
  - Do: Build zoom handler — resolve symbol via `resolve_symbol()`, extract body from source lines (0-based ranges), add context_before/context_after lines, walk AST for call expressions within symbol range (calls_out) and for calls to this symbol in other symbols (called_by). Handle language-specific call node kinds (TS/JS/Go: `call_expression`, Python: `call`, Rust: `call_expression` + `macro_invocation`). Use last-segment heuristic for member access calls. Create calls.ts fixture with intra-file function calls. Return `AmbiguousSymbol` when `resolve_symbol` returns multiple matches. Wire zoom into dispatch.
  - Verify: `cargo test --lib -- commands::zoom::tests` and `cargo test --test integration -- test_zoom` all pass, `cargo build` with 0 warnings
  - Done when: zoom command returns correct symbol body, context lines, calls_out, and called_by for test fixtures through the binary protocol; ambiguous symbol names produce structured error with candidates

## Files Likely Touched

- `src/commands/mod.rs` (new)
- `src/commands/outline.rs` (new)
- `src/commands/zoom.rs` (new)
- `src/main.rs` (modified — dispatch + provider wiring)
- `src/lib.rs` (modified — add commands module)
- `tests/fixtures/calls.ts` (new — zoom test fixture)
- `tests/integration/commands_test.rs` (new — integration tests for both commands)
