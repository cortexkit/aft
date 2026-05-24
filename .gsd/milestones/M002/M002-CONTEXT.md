# M002: Language Intelligence — Context

**Gathered:** 2026-03-14
**Status:** Ready for planning (after M001 completes)

## Project Description

Agent File Toolkit (AFT) — extending the M001 foundation with language-specific intelligence: import management, scope-aware insertion, compound operations, auto-formatting, full validation, and multi-file atomic transactions.

## Why This Milestone

M001 gives agents semantic editing — but edits still produce formatting inconsistencies, import errors, and partial failures across files. M002 eliminates these by adding language-aware operations that handle the mechanical details agents get wrong most often: import placement (~15% error rate), indentation in nested scopes, and multi-file atomicity.

## User-Visible Outcome

### When this milestone is complete, the user can:

- Add imports with a single command that handles grouping, deduplication, and alphabetization per language
- Insert class/struct members at the right position with correct indentation
- Apply language-specific transforms (add derives in Rust, wrap try/catch in TS, add decorators in Python, add struct tags in Go)
- Have every edit auto-formatted by the project's configured formatter
- Opt into full type-checker validation after edits
- Preview any edit as a dry-run diff before applying
- Apply multi-file edits atomically — all succeed or all roll back

### Entry point / environment

- Entry point: OpenCode tool calls (aft_add_import, aft_add_member, aft_compound, aft_transaction, etc.)
- Environment: local dev — OpenCode CLI with AFT plugin
- Live dependencies involved: external formatters (prettier, rustfmt, black/ruff, gofmt) and type checkers (tsc, pyright, cargo check, go vet) — invoked if available, gracefully skipped if not

## Completion Class

- Contract complete means: all commands produce correct results for valid inputs across all 6 languages, with tests per language
- Integration complete means: import management correctly interacts with existing import blocks in real codebases
- Operational complete means: auto-format detects project formatter config, transactions roll back cleanly on partial failure

## Final Integrated Acceptance

To call this milestone complete, we must prove:

- Agent adds an import to a TypeScript file that already has 3 import groups — new import lands in the correct group, is deduplicated, and the file is auto-formatted
- Agent applies a multi-file transaction across 3 files where the third file has a syntax error — all 3 files are rolled back to pre-transaction state
- Dry-run on an edit_symbol returns a correct diff preview without modifying the file

## Risks and Unknowns

- **Import grouping rules vary per project** — some projects use custom import ordering (e.g., internal packages first). Need to either detect project conventions or use language defaults.
- **External formatter/type checker availability** — graceful degradation when tools aren't installed. Must not fail hard.
- **Python indentation as scope** — indent-aware insertion is tricky. Off-by-one indentation breaks semantics.

## Existing Codebase / Prior Art

- M001 provides: persistent binary, tree-sitter parsing for 6 languages, symbol resolution, editing engine with auto-backup, checkpoint system
- OpenCode plugin bridge from M001 — extend with new tool registrations

> See `.gsd/DECISIONS.md` for all architectural and pattern decisions.

## Relevant Requirements

- R013 — Import management (primary)
- R014 — Scope-aware member insertion (primary)
- R015 — Language-specific compound operations (primary)
- R016 — Auto-format on save (primary)
- R017 — Full validation mode (primary)
- R018 — Dry-run mode (primary)
- R019 — Multi-file atomic transactions (primary)
- R034 — Web-first language priority (applies to import/compound op language ordering)

## Scope

### In Scope

- `add_import`, `remove_import`, `organize_imports` for all 6 languages
- `add_member` for classes, structs, impl blocks
- Compound operations: `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`
- Auto-format integration with project formatters
- Opt-in full validation with external type checkers
- `dry_run: true` on all mutation commands
- `transaction` for multi-file atomic edits

### Out of Scope / Non-Goals

- Call graph navigation (M003)
- Refactoring primitives (M004)
- User-extensible compound operation templates (deferred — R036)

## Technical Constraints

- Import grouping rules must be configurable but ship with sensible defaults per language
- External tool invocation (formatters, type checkers) must have timeout protection
- Transaction rollback uses the per-file backup system from M001
- All new commands extend the existing JSON protocol — no protocol changes

## Integration Points

- M001 editing engine — dry-run and transaction build on the existing edit infrastructure
- External formatters — prettier, rustfmt, black/ruff, gofmt (spawned as subprocesses)
- External type checkers — tsc, pyright, cargo check, go vet (spawned as subprocesses)
- Project config files — .prettierrc, rustfmt.toml, pyproject.toml, go.mod (detected for formatter/checker configuration)

## Open Questions

- **Import group detection vs convention** — should AFT analyze existing imports to infer grouping, or always use language-standard conventions? Language-standard is simpler and more predictable.
- **Formatter config discovery** — how far up the directory tree should AFT search for formatter configs? Standard: walk up to project root (where .git is).
