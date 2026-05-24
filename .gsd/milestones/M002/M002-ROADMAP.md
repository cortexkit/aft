# M002: Language Intelligence

**Vision:** Agents perform language-aware editing — imports, member insertion, structural transforms — with auto-formatting, validation, dry-run preview, and multi-file atomicity. The mechanical details agents get wrong most often (import placement, indentation, partial failures) are handled by AFT.

## Success Criteria

- Agent adds an import to a TypeScript file with 3 existing import groups — the new import lands in the correct group, is deduplicated if already present, and the file is auto-formatted
- Agent applies a multi-file transaction across 3 files where the third file has a syntax error — all 3 files are rolled back to their pre-transaction state
- Agent dry-runs an `edit_symbol` and receives a correct unified diff without the file being modified on disk
- Agent adds a method to a Python class with 4-space indentation — the new method matches the existing indentation exactly
- Agent adds `#[derive(Clone)]` to a Rust struct that already has `#[derive(Debug)]` — the derive is appended to the existing attribute, not duplicated
- Agent edits a file and the response includes `formatted: true` when prettier is available, or `formatted: false, reason: "prettier not found"` when it isn't

## Key Risks / Unknowns

- **Import grouping varies per language and per project** — Each language has different conventions (isort 3-group for Python, goimports for Go, no universal standard for TS/JS). Tree-sitter provides import node types but the grouping/dedup/sort logic is hand-rolled application code.
- **Python indentation as scope** — Inserting members into Python classes requires detecting and matching the existing indentation. Off-by-one breaks semantics. No existing infrastructure for indentation detection.
- **External tool availability and config discovery** — Formatters and type checkers may not be installed, configs may be at various levels in the directory tree, and subprocess invocation needs timeout protection without hanging the single-threaded binary.

## Proof Strategy

- Import grouping → retire in S01 by shipping `add_import`/`organize_imports` for all 6 languages with integration tests proving correct group placement, dedup, and alphabetization per language
- Python indentation → retire in S02 by shipping `add_member` for Python classes with integration tests proving correct indentation detection and matching
- External tool invocation → retire in S03 by shipping auto-format with graceful degradation, integration tests proving formatter found/not-found paths and subprocess timeout behavior

## Verification Classes

- Contract verification: Rust integration tests via `AftProcess` helper + plugin tests via BinaryBridge — per-command, per-language coverage
- Integration verification: full plugin→binary→response stack for all new commands; auto-format applied to import management results
- Operational verification: external tool timeout and kill behavior, formatter/type-checker not-found graceful degradation, transaction rollback on partial failure
- UAT / human verification: none — all criteria are machine-verifiable

## Milestone Definition of Done

This milestone is complete only when all are true:

- All 4 slices marked `[x]` in this roadmap
- All new commands (add_import, remove_import, organize_imports, add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags, transaction) work through the binary protocol AND through the OpenCode plugin as registered tools
- Dry-run mode works on ALL mutation commands (existing M001 commands + all new M002 commands)
- Auto-format hooks into the edit pipeline — every mutation auto-formats when a project formatter is detected
- `cargo test` and `bun test` pass with 0 failures
- `cargo build` produces 0 warnings
- Final integrated acceptance scenarios verified:
  - `add_import` to a TS file with 3 import groups → correct group, deduplicated, auto-formatted
  - `transaction` across 3 files, third fails syntax → all 3 rolled back
  - `edit_symbol` with `dry_run: true` → unified diff returned, file unchanged on disk

## Requirement Coverage

- Covers: R013, R014, R015, R016, R017, R018, R019
- Partially covers: R034 (web-first ordering applied within each slice's language implementation)
- Leaves for later: R020–R027 (M003), R028–R031/R033 (M004)
- Orphan risks: none

## Slices

- [x] **S01: Import Management** `risk:high` `depends:[]`
  > After this: agent calls `add_import` on a TypeScript file with 3 import groups and the new import lands in the correct group, alphabetized and deduplicated — proven by integration tests across all 6 languages
- [x] **S02: Scope-aware Insertion & Compound Operations** `risk:medium` `depends:[]`
  > After this: agent calls `add_member` to insert a method into a Python class and it appears at the correct indentation; `add_derive` appends to an existing Rust derive attribute — proven by integration tests
- [x] **S03: Auto-format & Validation** `risk:medium` `depends:[S01]`
  > After this: every mutation command auto-formats via the project's formatter when available, and `validate: "full"` invokes the project's type checker — proven by integration tests including formatter-not-found graceful degradation
- [x] **S04: Dry-run & Transactions** `risk:low` `depends:[]`
  > After this: agent previews any edit as a unified diff without modifying the file, and applies multi-file edits atomically with rollback on failure — proven by integration tests including the 3-file rollback acceptance scenario

## Boundary Map

### S01 → S03

Produces:
- Import management commands (`add_import`, `remove_import`, `organize_imports`) as command handlers in `src/commands/` following the `handle_*(req, ctx) -> Response` pattern
- Import block detection and manipulation functions (per-language import node location, group classification, deduplication, sort) — reusable for any future commands that need to understand imports
- Plugin tool registrations for all 3 import commands in `opencode-plugin-aft/src/tools/`

Consumes:
- nothing (first slice)

### S02 (standalone)

Produces:
- Indentation detection utility in `src/indent.rs` — detects file's indent style (tabs vs spaces, width) from existing content. Reusable by any command that inserts code.
- `add_member` command handler for inserting into classes/structs/impl blocks with correct indentation
- Four compound operation command handlers (`add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`)
- Plugin tool registrations for all 5 commands

Consumes:
- nothing (independent of S01)

### S03 → S04

Produces:
- External tool runner utility — subprocess spawn with configurable timeout, kill on timeout, graceful not-found handling. Reusable for any external tool invocation.
- Auto-format hook in the edit pipeline (`src/edit.rs`) — all mutation commands auto-format after edit when a formatter is detected. Formatter config detection walks up to project root (.git).
- `validate: "full"` flag support on all mutation commands — invokes external type checker and returns errors
- `format` and `validate` response fields on all mutation command responses

Consumes:
- S01's import commands exist so the integrated acceptance criterion (imports + auto-format) can be verified

### S04 (final)

Produces:
- `similar` crate dependency for unified diff generation
- `dry_run: true` flag on ALL mutation commands (M001's edit_symbol, edit_match, write, batch + all M002 commands) — returns unified diff without modifying file, validates proposed content syntax
- `transaction` command handler — multi-file atomic edits with backup-all → apply-all → validate-all → rollback-all-on-failure semantics
- Plugin tool registrations for transaction command and dry_run param on all existing tools

Consumes:
- nothing (operates on the shared edit pipeline; works with existing and new commands)
