---
estimated_steps: 5
estimated_files: 5
---

# T01: Outline command with dispatch wiring

**Slice:** S03 — Structural Reading
**Milestone:** M001

## Description

Establish the `src/commands/` module pattern and implement the `outline` command. This is the simpler of the two S03 commands — it transforms the flat symbol list from `TreeSitterProvider.list_symbols()` into a nested tree structure for the JSON response. Methods appear only under their parent class/struct, not duplicated at top level.

This task also handles the critical wiring change: making `_provider` in main.rs the active provider passed to command handlers via the dispatch function.

## Steps

1. Create `src/commands/mod.rs` — declare `outline` and `zoom` submodules (zoom as empty placeholder), export handler functions
2. Create `src/commands/outline.rs` — define `OutlineParams` (file: String) deserialized from request params, `OutlineEntry` struct (name, kind, range, signature, exported, members: Vec<OutlineEntry>) with Serialize. Implement `handle_outline(req: &RawRequest, provider: &dyn LanguageProvider) -> Response` that calls `list_symbols()`, builds nested tree by filtering top-level (parent.is_none()) and nesting children under parents, returns Response::success with entries array
3. Wire into `src/main.rs`: rename `_provider` to `provider`, change `dispatch` signature to accept `&dyn LanguageProvider`, add `"outline"` arm calling `commands::outline::handle_outline`
4. Update `src/lib.rs` to declare `pub mod commands`
5. Add unit tests in `outline.rs` and integration tests in `tests/integration/commands_test.rs` using AftProcess pattern — send outline command for sample.ts, verify nested structure (methods under UserService, not at top level), verify all symbol kinds present

## Must-Haves

- [ ] `src/commands/mod.rs` exists with module declarations
- [ ] `OutlineEntry` has recursive `members` field for nesting
- [ ] Methods with `parent.is_some()` appear ONLY under their parent, not at top level
- [ ] Multi-level nesting works (Python `OuterClass.InnerClass.inner_method`)
- [ ] All 7 SymbolKind variants handled (including TypeAlias)
- [ ] `_provider` renamed and passed through dispatch to handler
- [ ] Integration test proves outline works through binary protocol

## Verification

- `cargo test --lib -- commands::outline::tests` — all outline unit tests pass
- `cargo test --test integration -- test_outline` — integration test passes
- `cargo build` — 0 errors, 0 warnings
- All existing S01/S02 tests still pass: `cargo test`

## Inputs

- `src/parser.rs` — `TreeSitterProvider.list_symbols()` returns flat `Vec<Symbol>` with `parent` and `scope_chain` fields encoding hierarchy
- `src/language.rs` — `LanguageProvider` trait with `list_symbols(&self, file: &Path) -> Result<Vec<Symbol>, AftError>`
- `src/protocol.rs` — `RawRequest` with flattened params, `Response::success()`/`Response::error()`
- `src/main.rs` — current dispatch pattern with `_provider` unused
- `tests/integration/protocol_test.rs` — `AftProcess` pattern for integration tests
- Existing fixtures: `tests/fixtures/sample.ts`, `sample.py`, `sample.rs` (multi-symbol files for testing)

## Observability Impact

- **New error responses:** `"file_not_found"` when outline target file doesn't exist, `"invalid_request"` when `file` param is missing — both return structured JSON with `code` + `message` fields
- **Stderr logging:** `[aft]` prefix log on outline dispatch (file path) for tracing command flow; parser errors already logged by `TreeSitterProvider`
- **Inspection surface:** A future agent can send `{"id":"1","command":"outline","file":"path/to/file.ts"}` through the binary protocol and inspect the JSON response — nested `members` arrays prove the tree builder works, `kind` fields prove all symbol types flow through
- **Failure visibility:** Missing file → `file_not_found` error response; unparseable file → empty entries array (tree-sitter gracefully degrades); missing `file` param → `invalid_request` error response

## Expected Output

- `src/commands/mod.rs` — module declarations for outline and zoom
- `src/commands/outline.rs` — outline handler with OutlineEntry serializable type, flat-to-tree nesting, unit tests
- `src/main.rs` — provider wired into dispatch, outline command arm added
- `src/lib.rs` — commands module declared
- `tests/integration/commands_test.rs` — integration tests for outline command through binary protocol
