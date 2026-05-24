# S02: Scope-aware Insertion & Compound Operations

**Goal:** Agent calls `add_member` to insert methods/fields into classes/structs/impl blocks at the correct indentation, and uses language-specific compound operations (`add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`) — all through the binary protocol and OpenCode plugin.
**Demo:** `add_member` inserts a method into a Python class with 4-space indentation matching existing members; `add_derive` appends `Clone` to an existing `#[derive(Debug)]` attribute on a Rust struct — proven by integration tests.

## Must-Haves

- Shared `src/indent.rs` utility that detects a file's indentation style (tabs vs spaces, width) from existing content (D042)
- `add_member` command handles TS/JS, Python, Rust, and Go scope containers with correct indentation
- `add_member` supports positioning: `first`, `last`, `before:name`, `after:name`
- `add_member` handles empty scope containers (class/struct with no existing members)
- `add_derive` appends to existing `#[derive(...)]` or creates new attribute on Rust structs/enums
- `wrap_try_catch` wraps TS/JS function bodies with re-indented try/catch blocks
- `add_decorator` inserts Python decorators with correct indentation, handling already-decorated functions
- `add_struct_tags` adds/updates Go struct field tags
- All 5 commands registered in OpenCode plugin with Zod schemas
- Integration tests proving all 5 commands through the binary protocol
- `cargo test` and `bun test` pass with 0 failures, `cargo build` with 0 warnings

## Proof Level

- This slice proves: contract + integration
- Real runtime required: no (binary protocol tests via AftProcess)
- Human/UAT required: no

## Verification

- `cargo build 2>&1 | grep -c warning` → 0
- `cargo test` — all existing tests pass + new `add_member` and compound operation integration tests
- `cargo test -- member` — add_member tests covering all 4 language families, positioning, empty containers
- `cargo test -- structure` — compound operation tests covering add_derive, wrap_try_catch, add_decorator, add_struct_tags
- `bun test` in `opencode-plugin-aft/` — plugin tool schema registration tests for all 5 new commands
- Error responses include structured `code` field (`scope_not_found`, `member_not_found`, `invalid_request`) — verify via integration test assertions on error response shape

## Observability / Diagnostics

- Runtime signals: stderr log `[aft] add_member: {file}` / `[aft] add_derive: {file}` / etc. on every call
- Inspection surfaces: error responses with structured `code` + `message` (scope_not_found, member_not_found, invalid_request)
- Failure visibility: scope container resolution errors include the scope name searched and available scopes

## Integration Closure

- Upstream surfaces consumed: `src/edit.rs` (replace_byte_range, auto_backup, validate_syntax), `src/parser.rs` (FileParser, detect_language, grammar_for, LangId), `src/commands/add_import.rs` (handler pattern reference)
- New wiring introduced: 5 command handlers in dispatch table, `src/indent.rs` shared utility, plugin `structure.ts` tool definitions
- What remains before the milestone is truly usable end-to-end: S03 (auto-format + validation), S04 (dry-run + transactions)

## Tasks

- [x] **T01: Indentation utility and `add_member` command** `est:2h`
  - Why: Core R014 deliverable — scope-aware member insertion is the highest-value and hardest operation in this slice. The shared indentation utility (D042) is a prerequisite for everything else.
  - Files: `src/indent.rs`, `src/commands/add_member.rs`, `src/commands/mod.rs`, `src/main.rs`, `src/lib.rs`, `tests/fixtures/member_*.{ts,py,rs,go}`, `tests/integration/member_test.rs`, `tests/integration/main.rs`
  - Do: Build `detect_indent()` in `src/indent.rs` — analyze source lines to determine tabs vs spaces and width, with language-specific defaults (4 spaces for Python/Rust, 2 for TS/JS, tabs for Go). Build `add_member` handler: parse AST → find scope container by name (class/struct/impl) → detect body indentation from existing children → resolve position (first/last/before:name/after:name) → indent provided code → insert at byte offset → backup → write → validate. Make `node_text()` and `node_range()` `pub(crate)` in parser.rs. Handle empty scope containers. Create test fixtures for each language family and integration tests via AftProcess.
  - Verify: `cargo test -- member` passes; `cargo build` produces 0 warnings
  - Done when: `add_member` inserts methods/fields into TS/JS classes, Python classes, Rust impl blocks/structs, and Go structs with correct indentation and all position modes work

- [x] **T02: Compound operations — `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`** `est:2h`
  - Why: R015 deliverable — the four language-specific structural transforms agents do most clumsily. Each uses a distinct AST navigation pattern but follows the same handler structure.
  - Files: `src/commands/add_derive.rs`, `src/commands/wrap_try_catch.rs`, `src/commands/add_decorator.rs`, `src/commands/add_struct_tags.rs`, `src/commands/mod.rs`, `src/main.rs`, `tests/fixtures/structure_*.{rs,ts,py,go}`, `tests/integration/structure_test.rs`, `tests/integration/main.rs`
  - Do: Four handlers each following the command pattern. `add_derive`: walk backward from struct/enum to find preceding `attribute_item` siblings, append to existing `token_tree` or insert new `#[derive(...)]`. `wrap_try_catch`: find function/method `statement_block`, re-indent body +1 level, wrap in `try { ... } catch (error) { throw error; }`. Limit to functions with statement_block bodies (skip arrow functions without braces). `add_decorator`: find function/class def, insert `@decorator` line before it with matching indentation, handle already-decorated functions (decorated_definition parent). `add_struct_tags`: find field_declaration in struct, parse existing tag if present, add/update key-value pair in backtick-delimited tag string. Integration tests for each operation.
  - Verify: `cargo test -- structure` passes; `cargo build` produces 0 warnings
  - Done when: All 4 compound operations work through binary protocol with integration tests proving the AST-level mutations

- [x] **T03: Plugin tool registrations for all 5 commands** `est:45m`
  - Why: Without plugin registration, agents can't access S02 commands through OpenCode. Completes the integration surface.
  - Files: `opencode-plugin-aft/src/tools/structure.ts`, `opencode-plugin-aft/src/index.ts`, `opencode-plugin-aft/src/__tests__/structure.test.ts`
  - Do: Create `structure.ts` exporting `structureTools(bridge)` with Zod schemas for all 5 commands following the D034 pattern (`const z = tool.schema`). Wire into `index.ts` via `...structureTools(bridge)`. Write bun tests verifying tool registration and schema validation.
  - Verify: `cd opencode-plugin-aft && bun test` passes with all new tool tests
  - Done when: All 5 S02 commands appear as registered tools in the plugin with correct schemas

## Files Likely Touched

- `src/indent.rs` (new — shared indentation detection utility)
- `src/commands/add_member.rs` (new — scope-aware member insertion)
- `src/commands/add_derive.rs` (new — Rust derive attribute manipulation)
- `src/commands/wrap_try_catch.rs` (new — TS/JS try-catch wrapping)
- `src/commands/add_decorator.rs` (new — Python decorator insertion)
- `src/commands/add_struct_tags.rs` (new — Go struct tag manipulation)
- `src/commands/mod.rs` (add 5 module declarations)
- `src/main.rs` (add 5 dispatch arms)
- `src/lib.rs` (add `pub mod indent`)
- `src/parser.rs` (make `node_text`, `node_range` pub(crate))
- `tests/integration/member_test.rs` (new — add_member integration tests)
- `tests/integration/structure_test.rs` (new — compound operation integration tests)
- `tests/integration/main.rs` (register new test modules)
- `tests/fixtures/member_*.{ts,py,rs,go}` (new — member insertion fixtures)
- `tests/fixtures/structure_*.{rs,ts,py,go}` (new — compound operation fixtures)
- `opencode-plugin-aft/src/tools/structure.ts` (new — plugin tool definitions)
- `opencode-plugin-aft/src/index.ts` (wire structureTools)
- `opencode-plugin-aft/src/__tests__/structure.test.ts` (new — plugin tests)
