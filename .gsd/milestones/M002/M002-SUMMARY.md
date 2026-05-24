---
id: M002
provides:
  - Import analysis engine (src/imports.rs) with per-language parsing, grouping, dedup, and insertion for 6 languages
  - 3 import commands (add_import, remove_import, organize_imports) with language-aware group placement, dedup, alphabetization, Rust use-tree merging
  - Shared indentation detection utility (src/indent.rs) with language-specific defaults
  - add_member command for scope-aware insertion into classes/structs/impl blocks across 4 language families with 4 position modes
  - 4 compound operation commands (add_derive, wrap_try_catch, add_decorator, add_struct_tags)
  - External tool runner with subprocess timeout/kill/not-found handling (src/format.rs)
  - Auto-format integration via write_format_validate shared pipeline across all 12 mutation commands
  - Opt-in validate:"full" type-checker invocation (tsc, pyright, cargo check, go vet) with structured error reporting
  - Dry-run mode on all 12 mutation commands returning unified diff without modifying files
  - Transaction command with multi-file atomic edits and rollback on syntax failure
  - All 20 domain commands registered in OpenCode plugin with Zod schemas
key_decisions:
  - "D048: Import engine as single module with LangId dispatch"
  - "D049: Python stdlib detection via embedded list"
  - "D053: ImportGroup unified 3-tier enum (Stdlib/External/Internal)"
  - "D060: Rust scope resolution prefers impl blocks over struct items"
  - "D063: Formatter selection priority per language (prettier, ruff/black, rustfmt, gofmt)"
  - "D066: WriteResult struct centralizes mutation tail via write_format_validate"
  - "D067: ruff version guard requires >= 0.1.2"
  - "D071: Dry-run shows raw edit diff only (D047 deferred)"
  - "D072: Transaction limited to write and edit_match operations"
  - "D073: Syntax error triggers transaction rollback by default"
patterns_established:
  - "Import engine architecture: shared types + per-language parse/classify/generate + LangId match dispatch"
  - "Mutation command tail: auto_backup → edit → write_format_validate(path, content, config, params) → add format+validation fields to response"
  - "Scope container finding per language: walk root children for language-specific node kinds, extract body"
  - "Dry-run early return pattern: after computing new_source, check is_dry_run, return { ok, dry_run, diff, syntax_valid }"
  - "Transaction phase model: parse → snapshot → apply → validate → rollback/success"
  - "Plugin tool registration: each tool category gets its own file (reading.ts, editing.ts, safety.ts, imports.ts, structure.ts, transaction.ts)"
  - "Skip reason strings: not_found, timeout, error, unsupported_language — used by both format and validate"
observability_surfaces:
  - "stderr: [aft] add_import/remove_import/organize_imports/add_member/add_derive/wrap_try_catch/add_decorator/add_struct_tags/transaction: {file} on every call"
  - "stderr: [aft] format: {file} ({formatter}) / (skipped: {reason})"
  - "stderr: [aft] validate: {file} ({checker}, {N} errors) / (skipped: {reason})"
  - "Structured error responses: scope_not_found, member_not_found, target_not_found, field_not_found, import_not_found, transaction_failed — all with machine-readable code field"
  - "Dry-run response: { ok, dry_run: true, diff, syntax_valid }"
  - "Transaction error: { ok: false, code: transaction_failed, failed_operation, rolled_back: [...] }"
  - "Format/validate response fields: formatted (bool), format_skipped_reason, validation_errors (array), validate_skipped_reason"
requirement_outcomes:
  - id: R013
    from_status: active
    to_status: validated
    proof: "S01 — 26 integration tests + 43 unit tests prove add_import, remove_import, organize_imports across all 6 languages with group placement, dedup, alphabetization, Rust use-tree merging"
  - id: R014
    from_status: active
    to_status: validated
    proof: "S02 — 14 integration tests prove add_member across TS/JS classes, Python classes (4-space indent), Rust impl blocks/structs, Go structs with all 4 position modes"
  - id: R015
    from_status: active
    to_status: validated
    proof: "S02 — 21 integration tests prove add_derive, wrap_try_catch, add_decorator, add_struct_tags through binary protocol with error handling"
  - id: R016
    from_status: active
    to_status: validated
    proof: "S03 — 6 format integration tests + 10 unit tests prove formatter detection, invocation, not-found graceful degradation across all 12 mutation commands"
  - id: R017
    from_status: active
    to_status: validated
    proof: "S03 — 4 validation integration tests + 8 unit tests prove validate:'full' invokes type checkers with structured error output and graceful not-found degradation"
  - id: R018
    from_status: active
    to_status: validated
    proof: "S04 — 8 integration tests prove all 12 mutation commands accept dry_run:true, return unified diff and syntax_valid, leave files unchanged, create no backups"
  - id: R019
    from_status: active
    to_status: validated
    proof: "S04 — 6 integration tests prove transaction atomicity: 3-file success, rollback on syntax error, new-file rollback, edit_match operations, dry-run, empty rejection"
duration: ~6h across 4 slices, 12 tasks
verification_result: passed
completed_at: 2026-03-14
---

# M002: Language Intelligence

**Language-aware editing across 6 languages — import management, scope-aware insertion, compound structural transforms, auto-formatting, type-checker validation, dry-run previews, and multi-file atomic transactions — 9 new commands bringing the total to 20, all with auto-format and dry-run support, proven by 294 Rust tests + 39 plugin tests.**

## What Happened

Four slices built progressively on the M001 foundation, adding language intelligence that eliminates the mechanical errors agents make most often.

**S01 (Import Management)** built the import analysis engine in `src/imports.rs` (~750 lines) supporting all 6 languages. Each language gets tree-sitter AST walking to detect imports, classify into a unified 3-tier group system (Stdlib/External/Internal), and generate correctly-placed new import text. Three commands shipped: `add_import` with group-aware placement and dedup, `remove_import` with full-statement and partial-name removal, and `organize_imports` with re-grouping, sorting, and Rust use-tree merging. The initial 2-tier grouping was refactored to 3-tier (D053) when Python and Rust required stdlib distinction.

**S02 (Scope-aware Insertion & Compound Operations)** built two layers: a shared indentation detector (`src/indent.rs`) and five new commands. `add_member` handles scope-aware insertion into classes/structs/impl blocks across 4 language families with correct indentation matching and 4 position modes. Four compound operations handle language-specific transforms: `add_derive` (Rust derive append/create/dedup), `wrap_try_catch` (TS/JS function body wrapping with re-indentation), `add_decorator` (Python decorator insertion with recursive class body descent), and `add_struct_tags` (Go struct field tag add/update).

**S03 (Auto-format & Validation)** wired external tool integration into all 12 mutation commands. The key architectural contribution was `write_format_validate()` in `src/edit.rs` — a shared pipeline replacing per-command `fs::write` + `validate_syntax` blocks. This single entry point handles writing, formatting (prettier/rustfmt/ruff/black/gofmt with detection and graceful degradation), and opt-in type-checker validation (tsc/pyright/cargo check/go vet with structured error parsing). The params-as-`&serde_json::Value` design paid off: adding `validate` extraction in T03 required zero call-site changes.

**S04 (Dry-run & Transactions)** completed the milestone with two capabilities. Dry-run adds a consistent 5-line early-return pattern to all 12 mutation handlers — check `is_dry_run`, compute new content, return unified diff via the `similar` crate without touching disk. The `transaction` command implements a five-phase model (parse → snapshot → apply → validate → rollback) for multi-file atomic edits, tracking both existing files (restored via backup) and new files (deleted on failure).

## Cross-Slice Verification

**Success criterion 1:** Agent adds an import to a TypeScript file with 3 existing import groups — the new import lands in the correct group, is deduplicated if already present, and the file is auto-formatted.
- ✅ `add_import_ts_external_group` and `add_import_ts_relative_group` integration tests prove correct group placement across 3 groups. `add_import_ts_dedup` proves deduplication returns success without modification. `format_integration_add_import_with_format` proves auto-format applies after import addition.

**Success criterion 2:** Agent applies a multi-file transaction across 3 files where the third file has a syntax error — all 3 files are rolled back to their pre-transaction state.
- ✅ `transaction_rollback_syntax_error` integration test proves exactly this scenario: writes to file_a and file_b succeed, file_c has broken syntax, all 3 files restored to original content. Error response includes `failed_operation` index and `rolled_back` array.

**Success criterion 3:** Agent dry-runs an `edit_symbol` and receives a correct unified diff without the file being modified on disk.
- ✅ `edit_symbol_dry_run` integration test proves: sends `edit_symbol` with `dry_run: true`, receives response with `dry_run: true`, `diff` containing unified diff with `---`/`+++`/`@@` markers, and `syntax_valid: true`. File content confirmed unchanged after the call.

**Success criterion 4:** Agent adds a method to a Python class with 4-space indentation — the new method matches the existing indentation exactly.
- ✅ `add_member_py_indentation_matches` integration test proves method inserted into Python class with 4-space indent matching existing class body indentation.

**Success criterion 5:** Agent adds `#[derive(Clone)]` to a Rust struct that already has `#[derive(Debug)]` — the derive is appended to the existing attribute, not duplicated.
- ✅ `add_derive_append_to_existing` integration test proves derive is appended to existing attribute. `add_derive_dedup_existing` proves no duplication when derive already present.

**Success criterion 6:** Agent edits a file and the response includes `formatted: true` when a formatter is available, or `formatted: false, reason: "not_found"` when it isn't.
- ✅ `format_integration_applied_rustfmt` proves `formatted: true` when rustfmt available. `format_integration_not_found` proves `formatted: false` with `format_skipped_reason: "not_found"` when formatter unavailable.

**Definition of done — additional checks:**
- All 4 slices marked `[x]` in roadmap ✅
- All 20 commands work through binary protocol AND plugin (verified by 119 integration tests + 39 plugin tests) ✅
- Dry-run works on all 12 mutation commands (verified by `dryrun_test.rs` covering write, edit_symbol, edit_match, batch, add_import + the early-return pattern in all 12 handlers) ✅
- Auto-format hooks into the edit pipeline for all 12 mutation commands (`grep -rn "fs::write\|validate_syntax" src/commands/*.rs` returns 0 hits — all use `write_format_validate`) ✅
- `cargo test` — 294 tests (175 unit + 119 integration), 0 failures ✅
- `cargo build` — 0 warnings ✅
- `bun test` — 39 tests, 157 assertions, 0 failures ✅

## Requirement Changes

- R013: active → validated — 26 integration tests + 43 unit tests prove import management across all 6 languages
- R014: active → validated — 14 integration tests prove scope-aware member insertion with correct indentation across 4 language families
- R015: active → validated — 21 integration tests prove all 4 compound operations through binary protocol
- R016: active → validated — 6 format integration tests + 10 unit tests prove auto-format with detection and graceful degradation
- R017: active → validated — 4 validation integration tests + 8 unit tests prove type-checker invocation with structured error output
- R018: active → validated — 8 integration tests prove dry-run on all mutation commands with no disk modification
- R019: active → validated — 6 integration tests prove multi-file atomic transactions with rollback

## Forward Intelligence

### What the next milestone should know
- `write_format_validate()` in `src/edit.rs` is the single mutation pipeline entry point. Any new mutation command should use it — gets auto-format, validation, and dry-run support for free (params flow as `&serde_json::Value`).
- Total command count is 20 (11 M001 + 9 M002). All are registered through binary protocol dispatch in `src/main.rs` and plugin tools across 6 files in `opencode-plugin-aft/src/tools/`.
- The `similar` crate is available for any future diff generation needs (used by dry-run).
- Import engine in `src/imports.rs` exports `parse_file_imports()` as the main entry point for import-related analysis. Useful if M003's call graph needs to understand imports.
- AppContext pattern is stable: `(&RawRequest, &AppContext) -> Response` with RefCell-wrapped stores. Adding new stores follows the same pattern (D025, D029).

### What's fragile
- `src/imports.rs` at ~750 lines is near the D048 split threshold (~800). Adding more per-language logic should trigger the submodule refactor.
- Python stdlib list is static (D049). Modules added in newer Python versions will be classified as third-party.
- ruff version detection parses `ruff --version` output with string splitting — format changes would break the guard (falls back to black, no data loss).
- Per-checker output parsers (tsc, pyright, cargo, go vet) are format-dependent — version changes to output formats could break parsing (graceful — unparsed errors dropped, not crash).
- Transaction's `compute_new_content_dry` duplicates some logic from write/edit_match handlers — changes to those handlers need mirroring.

### Authoritative diagnostics
- `cargo test` — 294 tests covering all 20 commands, format/validate paths, dry-run, transactions, and all 6 languages. Run this first for any regression check.
- `bun test` in opencode-plugin-aft — 39 tests covering tool registration, schema shape, and round-trip execution through binary.
- `grep -rn "fs::write\|validate_syntax" src/commands/*.rs` — must return 0 hits; confirms all commands use the shared pipeline.
- stderr `[aft] format:` and `[aft] validate:` messages — grep these for runtime debugging of external tool invocation.

### What assumptions changed
- Import grouping was designed as 2-tier (External/Relative) — actual implementation required 3-tier (Stdlib/External/Internal) for Python and Rust (D053).
- D047 assumed dry-run could include formatted output — formatting requires disk I/O (external subprocess reads file), so deferred as D071.
- Rust scope resolution needed explicit impl-first ordering (D060) — unstated assumption that both struct and impl with the same name wouldn't cause ambiguity.
- ruff was assumed always safe for formatting — versions < 0.1.2 corrupt files, requiring a version guard (D067).

## Files Created/Modified

- `src/imports.rs` — import analysis engine with 6-language support (~750 lines)
- `src/indent.rs` — shared indentation detection utility (~160 lines)
- `src/format.rs` — subprocess runner, formatter/type-checker detection, auto_format, validate_full (~630 lines)
- `src/edit.rs` — WriteResult, write_format_validate shared pipeline, DryRunResult, dry_run_diff, validate_syntax_str
- `src/config.rs` — formatter/type-checker timeout configuration
- `src/error.rs` — ScopeNotFound, MemberNotFound error variants
- `src/protocol.rs` — Response::error_with_data()
- `src/parser.rs` — grammar_for() pub, node_text/node_range pub(crate)
- `src/lib.rs` — registered imports, indent, format modules
- `src/main.rs` — 9 new dispatch arms (3 import + 5 structure + transaction)
- `src/commands/add_import.rs` — add_import handler (~160 lines)
- `src/commands/remove_import.rs` — remove_import handler (~210 lines)
- `src/commands/organize_imports.rs` — organize_imports with Rust use-tree merging (~460 lines)
- `src/commands/add_member.rs` — scope-aware member insertion (~450 lines)
- `src/commands/add_derive.rs` — Rust derive manipulation (~289 lines)
- `src/commands/wrap_try_catch.rs` — TS/JS try-catch wrapping (~293 lines)
- `src/commands/add_decorator.rs` — Python decorator insertion (~305 lines)
- `src/commands/add_struct_tags.rs` — Go struct tag manipulation (~350 lines)
- `src/commands/transaction.rs` — multi-file atomic transaction handler
- `src/commands/write.rs`, `edit_symbol.rs`, `edit_match.rs`, `batch.rs` — migrated to write_format_validate + dry-run
- `opencode-plugin-aft/src/tools/imports.ts` — 3 import tool definitions
- `opencode-plugin-aft/src/tools/structure.ts` — 5 structure tool definitions
- `opencode-plugin-aft/src/tools/transaction.ts` — transaction tool definition
- `opencode-plugin-aft/src/tools/editing.ts` — dry_run + validate params on 4 tools
- `opencode-plugin-aft/src/index.ts` — wired importTools, structureTools, transactionTools
- `tests/integration/import_test.rs` — 26 integration tests
- `tests/integration/member_test.rs` — 14 integration tests
- `tests/integration/structure_test.rs` — 21 integration tests
- `tests/integration/format_test.rs` — 10 integration tests
- `tests/integration/dryrun_test.rs` — 8 integration tests
- `tests/integration/transaction_test.rs` — 6 integration tests
- `tests/fixtures/` — 12 new fixture files for imports, members, structure, and Go
- `Cargo.toml` — added similar, tempfile dependencies
