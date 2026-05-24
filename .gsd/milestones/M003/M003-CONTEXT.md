# M003: Call Graph Navigation — Context

**Gathered:** 2026-03-14
**Status:** Ready for planning (after M001 completes)

## Project Description

Agent File Toolkit (AFT) — extending the M001 foundation with static call graph construction and one-call code navigation primitives: forward call trees, reverse caller trees, reverse traces to entry points, data flow tracking, and change impact analysis.

## Why This Milestone

Tracing call chains is the single most token-expensive agent workflow (~5000 tokens per 4-file trace). M003 replaces this with single-call operations (~400 tokens). This is the biggest token savings in the entire AFT roadmap. M003 is independent of M002 — it builds on M001's tree-sitter parsing and symbol resolution, not on language intelligence.

## User-Visible Outcome

### When this milestone is complete, the user can:

- Ask "what does this function call?" and get a complete call tree to arbitrary depth in one call
- Ask "what calls this function?" and get all call sites grouped by file
- Ask "how does execution reach this function?" and get all paths from entry points rendered top-down with data threading
- Ask "if I change this function's signature, what breaks?" and get a complete impact analysis with suggestions
- Follow a specific value through function calls seeing type transformations and variable renames

### Entry point / environment

- Entry point: OpenCode tool calls (aft_call_tree, aft_callers, aft_trace_to, aft_trace_data, aft_impact)
- Environment: local dev — OpenCode CLI with AFT plugin
- Live dependencies involved: none (all analysis is static, based on tree-sitter parsing)

## Completion Class

- Contract complete means: all navigation commands return correct results for known call patterns, with test suites covering direct calls, method calls, chained calls, and cross-file references
- Integration complete means: call graph correctly resolves imports/exports across files in real multi-file projects
- Operational complete means: file watcher correctly invalidates stale graph nodes, lazy construction handles first-query cold start within 2s for typical projects

## Final Integrated Acceptance

To call this milestone complete, we must prove:

- Agent runs `trace_to` on a deeply-nested utility function in a real multi-file project and gets correct paths from entry points (route handlers, event listeners) with data threading
- File watcher detects a modified file and the next `callers` query reflects the change
- `impact` on a function with 5+ callers across 3+ files returns all affected call sites with correct suggestions

## Risks and Unknowns

- **Call graph accuracy for dynamic languages** — JavaScript/Python have dynamic dispatch, higher-order functions, and computed property access that static analysis cannot fully resolve. The call graph will be approximate for these patterns.
- **Performance on large codebases** — lazy construction helps, but a deeply-nested trace_to could still scan hundreds of files on first query. Need depth limits and caching.
- **Cross-file symbol resolution** — following import/export chains across files is non-trivial, especially with re-exports, barrel files, and aliased imports.
- **Entry point detection reliability** — heuristics for route handlers, event listeners, etc. are framework-specific. May need framework-specific patterns (Express vs Koa vs Hono, Flask vs Django vs FastAPI).

## Existing Codebase / Prior Art

- M001 provides: persistent binary with tree-sitter parsing, symbol extraction, `FileParser` with parse tree caching, `LanguageProvider` trait
- M001's `zoom` command already does basic caller/callee annotation — M003 extends this to full graph navigation
- The persistent process architecture is ideal for call graph caching — graph lives in memory between queries

> See `.gsd/DECISIONS.md` for all architectural and pattern decisions.

## Relevant Requirements

- R020 — Call graph construction with lazy building, incremental updates, and file watcher (primary)
- R021 — Forward call tree (primary)
- R022 — Reverse caller tree (primary)
- R023 — Reverse trace to entry points (primary)
- R024 — Data flow tracking (primary)
- R025 — Change impact analysis (primary)
- R026 — Entry point detection heuristics (primary)
- R027 — Worktree-aware scoping (primary)

## Scope

### In Scope

- Static call graph construction using tree-sitter (function calls, method calls, property access chains)
- Lazy/incremental graph building — scan on demand, cache results
- File watcher for graph invalidation
- Worktree-aware scoping (.gitignore respected, node_modules/target/venv excluded)
- `call_tree`, `callers`, `trace_to`, `trace_data`, `impact` commands
- Entry point detection heuristics for common frameworks
- Cross-file symbol resolution via import/export following
- Graph caching in persistent process memory

### Out of Scope / Non-Goals

- Dynamic dispatch resolution (call graph is static/approximate)
- Runtime profiling or tracing
- Call graph persistence to disk (deferred — R037)
- Type-level analysis (that's LSP's job)

## Technical Constraints

- Call graph is tree-sitter-based — approximate, not type-aware. Accuracy target: ~80% for direct calls, lower for dynamic patterns.
- File watcher must not overwhelm the persistent process — debounce file changes, batch graph updates
- Depth limits on all graph traversals to prevent explosion
- Worktree root detected from .git location or OpenCode's worktree context
- Graph invalidation is per-file — when a file changes, all its nodes are rebuilt

## Integration Points

- M001 `FileParser` — reuse parsed trees for call site extraction
- M001 `LanguageProvider` — symbol resolution for cross-file references
- M001 protocol — new commands extend the existing JSON protocol
- File system watcher — `notify` Rust crate (or similar) for file change detection
- `.gitignore` — respect ignore patterns for scoping

## Open Questions

- **Framework-specific entry point patterns** — should we ship with patterns for popular frameworks (Express, Flask, Axum, etc.) or start with generic patterns (exported functions, main, test)? Recommend starting generic, adding framework patterns as needed.
- **Higher-order function handling** — when a function is passed as a callback, should the call graph include the callback's callers? This is hard to do statically. Recommend best-effort with explicit "approximate" markers.
- **Graph memory budget** — for very large codebases, the in-memory graph could grow large. Need a strategy for eviction or limiting scope. Recommend LRU eviction of least-recently-accessed file nodes.
