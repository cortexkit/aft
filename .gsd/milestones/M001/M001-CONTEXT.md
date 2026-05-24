# M001: Foundation — Context

**Gathered:** 2026-03-14
**Status:** Ready for planning

## Project Description

Agent File Toolkit (AFT) — a Rust binary + OpenCode TypeScript plugin that gives AI coding agents semantic file manipulation and code navigation primitives. The binary uses tree-sitter for 6 languages (TS/JS/TSX, Python, Rust, Go) and communicates via JSON over stdin/stdout as a persistent process. The plugin is a thin bridge that registers tools with OpenCode and manages the binary lifecycle.

## Why This Milestone

Foundation must ship first because every subsequent milestone depends on the binary infrastructure, tree-sitter parsing, and the editing engine. Without the persistent process protocol, there's nothing to extend. Without tree-sitter symbol extraction, there's no semantic editing. Without the plugin bridge, agents can't access any of it.

## User-Visible Outcome

### When this milestone is complete, the user can:

- Install the OpenCode plugin and use AFT tools in their agent workflow
- Edit files by symbol name (`edit_symbol`), by content match (`edit_match`), or bulk write — all through OpenCode tool calls
- Get a file's structural outline or zoom into a specific function with caller/callee annotations
- Checkpoint the workspace before risky changes and restore if needed
- Undo individual file edits with a single command
- Install the binary via `npm install @aft/core` on any supported platform

### Entry point / environment

- Entry point: OpenCode tool calls (agent invokes `aft_edit_symbol`, `aft_outline`, etc.)
- Environment: local dev — OpenCode CLI with AFT plugin installed
- Live dependencies involved: none (all computation is local, tree-sitter grammars embedded in binary)

## Completion Class

- Contract complete means: all commands produce correct JSON responses for valid inputs, handle errors gracefully, and pass unit + integration tests
- Integration complete means: OpenCode plugin successfully spawns the binary, registers all tools, and agents can use them in real conversations
- Operational complete means: binary stays alive between commands, recovers from crashes via plugin restart, checkpoint store persists across sessions

## Final Integrated Acceptance

To call this milestone complete, we must prove:

- An agent in OpenCode can outline a TypeScript file, zoom to a function, edit it by symbol name, and verify the syntax is valid — all through tool calls, with the binary staying alive between calls
- Checkpoint/restore round-trips correctly — checkpoint, make edits, restore, verify files are back to original state
- `npm install @aft/core` on macOS ARM installs the binary and the plugin can locate and spawn it

## Risks and Unknowns

- **Tree-sitter symbol extraction accuracy across languages** — query patterns may need significant per-language tuning. TS/JS/TSX share patterns, but Python (indent-based scope), Rust (impl blocks, trait impls), and Go (interface embedding) each have unique challenges. This is the core bet of the project.
- **Persistent process protocol reliability** — the binary must handle malformed JSON, oversized inputs, concurrent-feeling rapid requests, and graceful shutdown without deadlocking or corrupting state.
- **Cross-compilation for 5 platforms** — Rust cross-compilation with embedded tree-sitter grammars may hit linking issues on some platforms, particularly Windows and Linux ARM.
- **OpenCode plugin API stability** — the plugin API is relatively new. Breaking changes could require adaptation.

## Existing Codebase / Prior Art

- Greenfield project — no existing code. Empty git repository.
- OpenCode plugin API uses `@opencode-ai/plugin` package — `tool()` helper with Zod schemas, async `execute(args, context)`, context includes `directory` and `worktree`.
- Tree-sitter Rust crate: `tree-sitter` with per-language grammar crates (`tree_sitter_typescript`, `tree_sitter_javascript`, `tree_sitter_python`, `tree_sitter_rust`, etc.). Queries use S-expression patterns with `@capture` names.
- Binary distribution pattern: esbuild, turbo, oxc all use npm optionalDependencies with platform-specific packages.

> See `.gsd/DECISIONS.md` for all architectural and pattern decisions — it is an append-only register; read it during planning, append to it during execution.

## Relevant Requirements

- R001 — Persistent binary architecture (primary)
- R002 — Multi-language tree-sitter parsing (primary)
- R003 — Structural reading (primary)
- R004 — Semantic editing (primary)
- R005 — Structural editing (primary)
- R006 — Bulk and batch editing (primary)
- R007 — Per-file auto-backup and undo (primary)
- R008 — Workspace-wide checkpoints (primary)
- R009 — OpenCode plugin bridge (primary)
- R010 — Post-edit syntax validation (primary)
- R011 — Symbol disambiguation (primary)
- R012 — Binary distribution (primary)
- R031 — LSP-aware architecture — provider interface established in this milestone
- R032 — Structured JSON I/O (primary)
- R034 — Web-first language priority (primary)

## Scope

### In Scope

- Rust binary with persistent JSON-over-stdin/stdout protocol
- Tree-sitter integration for 6 languages (web-first: TS/JS/TSX → Python → Rust → Go)
- `outline`, `zoom`, `edit_symbol`, `edit_match`, `write`, `batch` commands
- Per-file undo stack and workspace checkpoint system
- Post-edit tree-sitter syntax validation
- Symbol disambiguation flow
- OpenCode plugin with binary process management and tool registration
- npm platform packages for 5 targets + cargo install fallback
- LSP-aware provider interface (tree-sitter implementation only — LSP wiring deferred to M004)

### Out of Scope / Non-Goals

- Import management (M002)
- Auto-format on save (M002)
- Full type-checker validation (M002)
- Dry-run mode (M002)
- Multi-file transactions (M002)
- Call graph construction or navigation (M003)
- Refactoring primitives (M004)
- LSP integration wiring (M004 — interface defined here, implementation there)

## Technical Constraints

- Rust binary must be a single static binary with zero runtime dependencies
- All content passes through JSON stdin/stdout — no shell argument strings for code content
- Tree-sitter grammars are embedded in the binary at compile time
- Persistent process: one JSON object per line protocol (newline-delimited JSON)
- Checkpoint and backup storage in `.aft/` directory (gitignored)
- Plugin uses `@opencode-ai/plugin` package — tools defined with `tool()` helper and Zod schemas

## Integration Points

- OpenCode plugin API — tool registration, execution context (directory, worktree)
- Tree-sitter grammar crates — `tree_sitter_typescript`, `tree_sitter_javascript`, `tree_sitter_python`, `tree_sitter_rust`, `tree-sitter-go` (exact crate names to be verified during implementation)
- npm registry — platform package publishing for binary distribution
- GitHub Actions — CI cross-compilation pipeline

## Open Questions

- **Tree-sitter query patterns for Go interfaces** — Go's implicit interface satisfaction makes call graph construction harder. May need special handling in M003, but symbol extraction for M001 should be straightforward.
- **TSX/JSX tree-sitter grammar** — TypeScript's tree-sitter grammar handles TSX, but the exact query patterns for JSX components vs regular functions need testing.
- **Checkpoint storage format** — simple file copies vs delta-based storage. File copies are simpler and sufficient for M001. Delta storage could be added later if checkpoint sizes become a problem.
