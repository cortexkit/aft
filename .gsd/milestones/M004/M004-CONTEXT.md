# M004: Refactoring Primitives — Context

**Gathered:** 2026-03-14
**Status:** Ready for planning (after M002 and M003 complete)

## Project Description

Agent File Toolkit (AFT) — the final milestone, adding workspace-wide refactoring primitives (move symbol, extract function, inline symbol) and wiring LSP integration through the plugin mediation layer established in M001.

## Why This Milestone

Move symbol, extract function, and inline symbol are the refactoring operations agents currently do in 5-10 manual steps with high error rates. Each requires coordinating symbol resolution (M001), import management (M002), and call graph awareness (M003) — which is why this milestone comes last. LSP integration through plugin mediation completes the accuracy story.

## User-Visible Outcome

### When this milestone is complete, the user can:

- Move a function from one file to another with a single command — all imports across the workspace are automatically updated
- Extract a code range into a new function with auto-detected parameters and return type
- Inline a function call, replacing it with the function's body
- Get LSP-enhanced symbol resolution when a language server is available through OpenCode, improving accuracy from ~80% to ~99%

### Entry point / environment

- Entry point: OpenCode tool calls (aft_move_symbol, aft_extract_function, aft_inline_symbol)
- Environment: local dev — OpenCode CLI with AFT plugin
- Live dependencies involved: OpenCode's LSP infrastructure (for optional enhanced resolution)

## Completion Class

- Contract complete means: refactoring operations produce correct results on test cases covering common patterns per language
- Integration complete means: move_symbol correctly updates imports across a real multi-file project, LSP hints improve resolution accuracy
- Operational complete means: none (no new operational concerns beyond M001-M003)

## Final Integrated Acceptance

To call this milestone complete, we must prove:

- Agent moves a function from a service file to a utils file in a real project — all importing files are updated, no broken references
- Agent extracts a 15-line block with 3 free variables into a new function — parameters, return type, and call site replacement are all correct
- With LSP running, `edit_symbol` resolves an ambiguous symbol that tree-sitter alone couldn't disambiguate

## Risks and Unknowns

- **Import rewiring completeness** — move_symbol must find ALL files that import the moved symbol. Depends on call graph (M003) being accurate. Barrel files and re-exports add complexity.
- **Extract function parameter inference** — free variable detection must account for closures, class instance variables (`this`/`self`), and module-level variables that don't need to become parameters.
- **LSP protocol compatibility** — OpenCode's LSP infrastructure may expose data in a specific format. The plugin mediation layer needs to translate between OpenCode's LSP representation and the binary's `lsp_hints` fields.
- **Scope conflicts during inline** — inlining a function may introduce variable name collisions in the target scope. Need renaming strategy.

## Existing Codebase / Prior Art

- M001: persistent binary, tree-sitter parsing, symbol resolution with LSP-ready provider interface, editing engine, backup system
- M002: import management (add/remove/organize), scope-aware insertion, transactions
- M003: call graph (callers, cross-file symbol resolution, import/export following)
- The `LanguageProvider` trait from M001 has the LSP-ready interface — M004 wires it up

> See `.gsd/DECISIONS.md` for all architectural and pattern decisions.

## Relevant Requirements

- R028 — Move symbol with import rewiring (primary)
- R029 — Extract function (primary)
- R030 — Inline symbol (primary)
- R033 — LSP integration via plugin mediation (primary)
- R031 — LSP-aware architecture — completing the interface established in M001

## Scope

### In Scope

- `move_symbol` — move function/class/type between files, update all imports workspace-wide
- `extract_function` — extract line range into new function with auto-detected params/returns
- `inline_symbol` — replace function call with body, handle scope conflicts
- LSP integration — plugin queries OpenCode's LSP, passes `lsp_hints` to binary for enhanced resolution
- Update all existing commands to use LSP hints when available

### Out of Scope / Non-Goals

- Rename symbol (could be added but not in current requirements)
- Automated test generation for refactored code
- Refactoring suggestions (AFT executes refactors, doesn't suggest them)

## Technical Constraints

- move_symbol depends on M002's import management and M003's call graph — both must be complete
- extract_function must work without call graph (it's a local operation) but benefits from it for naming suggestions
- LSP data flows: OpenCode LSP → plugin TypeScript → JSON `lsp_hints` field → binary Rust → enhanced `LanguageProvider` resolution
- Binary never connects to language servers directly — plugin mediates all LSP communication

## Integration Points

- M002 import management — move_symbol uses add_import/remove_import to update references
- M003 call graph — move_symbol uses callers to find all files that reference the moved symbol
- M003 cross-file resolution — ensures all references are found, including re-exports
- OpenCode LSP infrastructure — plugin reads LSP data (symbol locations, type info, references)
- M001 `LanguageProvider` trait — LSP-enhanced implementation added alongside tree-sitter default

## Open Questions

- **OpenCode LSP data format** — what data does OpenCode expose from running language servers? Need to investigate the plugin context's LSP access during M004 planning.
- **Inline scope conflict resolution** — when inlining creates variable name collisions, should we auto-rename (risky) or report the conflict and let the agent decide? Recommend reporting with suggestions.
- **Partial move** — should move_symbol support moving individual methods out of a class? This is significantly more complex than moving top-level symbols. Recommend starting with top-level only.
