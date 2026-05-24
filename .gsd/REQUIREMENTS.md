# Requirements

This file is the explicit capability and coverage contract for the project.

Use it to track what is actively in scope, what has been validated by completed work, what is intentionally deferred, and what is explicitly out of scope.

## Active

### R001 — Persistent binary architecture
- Class: core-capability
- Status: validated
- Description: The aft Rust binary runs as a persistent process, receiving JSON commands on stdin and writing JSON responses to stdout. Keeps parse state, caches, and edit history in memory between calls.
- Why it matters: Per-command spawning would re-parse grammars and lose all cached state on every call, defeating incremental parsing and call graph caching.
- Source: user
- Primary owning slice: M001/S01
- Supporting slices: none
- Validation: S01 — 120 sequential commands without restart, malformed JSON recovery, clean shutdown on EOF
- Notes: Process must handle graceful shutdown, crash recovery signaling to the plugin.

### R002 — Multi-language tree-sitter parsing
- Class: core-capability
- Status: validated
- Description: Tree-sitter grammars embedded for TypeScript, JavaScript, TSX/JSX, Python, Rust, and Go. Language auto-detected from file extension. Symbol extraction queries per language for functions, classes, methods, structs, interfaces, enums.
- Why it matters: This is the foundation for every semantic operation — editing by symbol, outline, zoom, call graph all depend on accurate symbol extraction.
- Source: user
- Primary owning slice: M001/S02
- Supporting slices: none
- Validation: S02 — 53 unit tests prove symbol extraction across all 6 languages with representative code patterns. All symbol kinds, scope chains, export detection, arrow functions, impl blocks, receiver methods, decorated functions verified.
- Notes: Web-first priority — TS/JS/TSX share query patterns and ship first. Python next, then Rust and Go.

### R003 — Structural reading (outline + zoom)
- Class: primary-user-loop
- Status: validated
- Description: `outline` returns a file's structural overview — all symbols with kind, name, range, signature, export status, and member nesting. `zoom` returns a single symbol's full body with surrounding context and caller/callee annotations.
- Why it matters: Replaces reading a 500-line file to find 5 lines. Agent gets targeted information in ~50 lines instead of ~500.
- Source: user
- Primary owning slice: M001/S03
- Supporting slices: none
- Validation: S03 — 19 unit tests + 8 integration tests verify nested outline structure, all symbol kinds, call annotations, context lines, error paths, and multi-language fixtures (TS, Python, Rust).
- Notes: zoom annotations include both outbound calls (what this function calls) and inbound callers (what calls this function). Annotations are file-scoped; cross-file deferred to M003.

### R004 — Semantic editing (edit by symbol name)
- Class: core-capability
- Status: validated
- Description: `edit_symbol` accepts a symbol name and operation (replace, delete, insert_before, insert_after) and applies the edit to the resolved symbol's range. Returns the new range, syntax validation result, and backup ID.
- Why it matters: Agents think "change function X" but current tools require translation to line numbers. Symbol-level addressing eliminates that translation and its error rate.
- Source: user
- Primary owning slice: M001/S05
- Supporting slices: none
- Validation: S05 — integration tests prove edit_symbol replace/delete operations, auto-backup with undo round-trip, syntax validation (valid and intentional errors), and structured disambiguation with candidates (name, qualified, line, kind) using ambiguous.ts fixture.
- Notes: Must handle disambiguation when multiple symbols match (return candidates with qualified names).

### R005 — Structural editing (edit by content match)
- Class: core-capability
- Status: validated
- Description: `edit_match` targets lines by their content — single line match/replace or range match (from/to markers). When match is ambiguous (appears multiple times), returns all occurrences with ±2 lines context for the agent to choose by index.
- Why it matters: Useful when the agent already knows what the code looks like from earlier reads or conversation context. Faster than symbol-level for simple constant changes.
- Source: user
- Primary owning slice: M001/S05
- Supporting slices: none
- Validation: S05 — integration tests prove single match replacement, multiple occurrence disambiguation with context lines, occurrence index selection, no-match error, and empty-match rejection.
- Notes: Complements edit_symbol — agent picks whichever is more natural for the specific edit.

### R006 — Bulk and batch editing
- Class: core-capability
- Status: validated
- Description: `write` does full file write via JSON stdin (new files or complete rewrites). `batch` applies multiple edits to one file in a single call, auto-sorted bottom-to-top to prevent line drift. Batch is atomic — all succeed or all roll back.
- Why it matters: write eliminates shell escaping issues for file content. batch prevents the line-drift problem when making multiple edits in one file.
- Source: user
- Primary owning slice: M001/S05
- Supporting slices: none
- Validation: S05 — integration tests prove write creates/overwrites files, batch applies multiple match and line-range edits atomically, batch rolls back on any edit failure (file unchanged), and batch undo restores original via single backup.
- Notes: All content passes through JSON — no shell argument strings.

### R007 — Per-file auto-backup and undo
- Class: failure-visibility
- Status: validated
- Description: Every mutation operation automatically snapshots the affected file before modifying it. `undo` restores the previous version. `edit_history` shows the stack of edits per file with timestamps and operation descriptions.
- Why it matters: Agents make mistakes. A one-step undo per file means recovery costs ~50 tokens instead of ~1500 (re-read + re-edit).
- Source: user
- Primary owning slice: M001/S04
- Supporting slices: M001/S05
- Validation: S04 built BackupStore with snapshot/restore/history and protocol commands. S05 proves every mutation (write, edit_symbol, edit_match, batch) auto-snapshots before modification. Integration tests confirm undo restores pre-mutation state for all four commands.
- Notes: Undo stack is in-memory in the persistent process.

### R008 — Workspace-wide checkpoints
- Class: failure-visibility
- Status: validated
- Description: `checkpoint` creates a named snapshot of all tracked files. `restore_checkpoint` rolls back to a named checkpoint. `list_checkpoints` shows available checkpoints with file counts. Auto-cleanup after 24h (configurable).
- Why it matters: Agents need "save game" before risky multi-file changes. Workspace-level rollback is the safety net for experimental refactors.
- Source: user
- Primary owning slice: M001/S04
- Supporting slices: none
- Validation: S04 — CheckpointStore with create/restore/list/cleanup, all 5 commands wired through protocol, integration tests prove checkpoint→modify→restore cycle, name overwrite, TTL cleanup, and error paths. S05 mutation commands now provide the file-modification surface that checkpoints protect against.
- Notes: Stored in-memory (persistent process). TTL cleanup unit tested.

### R009 — OpenCode plugin bridge
- Class: integration
- Status: validated
- Description: TypeScript plugin registers all AFT commands as OpenCode tools with Zod schemas. Binary bridge manages the persistent process (spawn, health check, restart on crash). Platform binary resolver finds the correct binary (npm package, PATH, cargo).
- Why it matters: Without the plugin, agents can't access AFT tools. The bridge must handle process lifecycle robustly — crashed binary should auto-restart transparently.
- Source: user
- Primary owning slice: M001/S06
- Supporting slices: none
- Validation: S06 — 9 integration tests (51 assertions) prove: resolver finds binary, bridge spawns and manages process lifecycle (ping, crash auto-restart with backoff, clean shutdown), 4 tool round-trips (outline, write, edit_symbol, undo) through full plugin→binary→response stack. All 11 tools registered with Zod schemas, type-checked at build time.
- Notes: Plugin is intentionally thin — all logic lives in the Rust binary.

### R010 — Post-edit syntax validation
- Class: quality-attribute
- Status: validated
- Description: Every edit response includes a `syntax_valid` field from a tree-sitter re-parse (~1ms). This is the default validation level. Catches malformed code immediately after edit.
- Why it matters: Agents often produce syntax errors. Immediate feedback prevents cascading failures from building on broken code.
- Source: user
- Primary owning slice: M001/S05
- Supporting slices: none
- Validation: S05 — all four mutation commands return `syntax_valid` boolean. Integration tests prove valid code returns true, intentionally broken syntax returns false, unsupported languages return null. Fresh FileParser used per validation (D023).
- Notes: This is only tree-sitter syntax checking. Full type-checker validation is R017 (M002).

### R011 — Symbol disambiguation
- Class: quality-attribute
- Status: validated
- Description: When edit_symbol or zoom targets a symbol name that matches multiple symbols (e.g., two `validate` functions in different scopes), the response returns an `ambiguous_symbol` error with a candidates list showing qualified names, files, and line numbers. Agent reissues with the qualified name.
- Why it matters: Ambiguous symbols are a common source of wrong-target edits. Explicit disambiguation prevents silent wrong-function edits.
- Source: user
- Primary owning slice: M001/S05
- Supporting slices: M001/S03
- Validation: S05 — integration test with ambiguous.ts fixture proves disambiguation returns structured candidates with name, qualified name, line, and kind. Agent can reissue with scope parameter to select specific symbol.
- Notes: Qualified names use scope chain: `ClassName.methodName`, `module::function`.

### R012 — Binary distribution
- Class: launchability
- Status: validated
- Description: npm optional dependency packages per platform (darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64) following the esbuild/turbo pattern. CI pipeline cross-compiles for all 5 platforms. Fallback to `cargo install aft`.
- Why it matters: Users must be able to install AFT with a single `npm install` or `bun install`. Manual binary management is a non-starter for adoption.
- Source: user
- Primary owning slice: M001/S07
- Supporting slices: none
- Validation: S07 — 5 platform npm packages with correct os/cpu fields verified by validate-packages.mjs, resolver unit tests (13/13) prove platform mapping and fallback chain (npm → PATH → cargo), CI workflow defines 5-platform cross-compilation pipeline with ordered npm publish, Cargo.toml has crates.io metadata for cargo install fallback, release profile produces optimized 6.4MB binary.
- Notes: @aft/core declares optionalDependencies on all platform packages. npm resolves the correct one at install time.

### R013 — Import management (6 languages)
- Class: core-capability
- Status: validated
- Description: `add_import` adds imports with language-aware placement (TS named/default/type imports, Rust use tree merging, Python isort groups, Go goimports groups). `remove_import` cleans up after refactors. `organize_imports` re-sorts and re-groups all imports.
- Why it matters: Import management is high-error-rate (~15% wrong group, duplicates) and high-frequency. Language-aware automation eliminates these errors.
- Source: user
- Primary owning slice: M002/S01
- Supporting slices: none
- Validation: S01 — 26 integration tests prove add_import group placement, dedup, alphabetization across all 6 languages; remove_import full-statement and partial-name removal; organize_imports re-grouping, sorting, dedup, and Rust use-tree merging. 43 unit tests cover parsing and classification. Plugin registration verified by 22 bun tests.
- Notes: Must handle deduplication, alphabetization, and group separation per language convention.

### R014 — Scope-aware member insertion
- Class: core-capability
- Status: validated
- Description: `add_member` inserts methods, fields, or functions into the correct scope of a class/struct/impl with correct indentation. Supports positioning: after/before specific member, first, or last.
- Why it matters: Manual member insertion requires calculating indentation and finding the right insertion point — error-prone for agents.
- Source: user
- Primary owning slice: M002/S02
- Supporting slices: none
- Validation: S02 — 14 integration tests prove add_member across TS/JS classes, Python classes (4-space indent verified), Rust impl blocks/structs, and Go structs with all 4 position modes (first, last, before:name, after:name), empty containers, and structured error responses (scope_not_found with available list, member_not_found). Plugin round-trip verified by bun tests.
- Notes: Rust must distinguish between `impl Struct` and `impl Trait for Struct`. Python uses indentation as scope delimiter. Impl blocks preferred over struct items when both share a name (D060).

### R015 — Language-specific compound operations
- Class: differentiator
- Status: validated
- Description: Structural transforms that are idiomatic per language: add_derive (Rust), wrap_try_catch (TS/JS), add_decorator (Python), add_struct_tags (Go). Each modifies the AST structurally rather than string-matching.
- Why it matters: These are the operations agents do most clumsily — adding a derive to an existing attribute list, wrapping a function body in try/catch without breaking indentation.
- Source: user
- Primary owning slice: M002/S02
- Supporting slices: none
- Validation: S02 — 21 integration tests prove all 4 compound operations through binary protocol: add_derive append/create/dedup (5 tests), wrap_try_catch simple/class/custom-catch (4 tests), add_decorator plain/nested/positioned (5 tests), add_struct_tags add/update/preserve (7 tests). Plugin round-trip verified by bun tests. All return structured error codes (target_not_found, field_not_found) with available target lists.
- Notes: Hardcoded per language for now. Extensibility is deferred (R036). wrap_try_catch limited to statement_block bodies (D058).

### R016 — Auto-format on save
- Class: quality-attribute
- Status: validated
- Description: After every edit, detect and invoke the project's canonical formatter (prettier, rustfmt, black/ruff, gofmt) if available. Response indicates "applied" or "not_found". Agent never thinks about code style.
- Why it matters: Inconsistent formatting creates noise in diffs and wastes agent attention on style.
- Source: user
- Primary owning slice: M002/S03
- Validation: S03 — 6 format integration tests prove formatter detection/invocation/not-found across all 12 mutation commands via binary protocol. Response includes `formatted` (bool) and `format_skipped_reason`. 10 unit tests cover subprocess timeout/kill/not-found/happy-path.

### R017 — Full validation mode (opt-in type checkers)
- Class: quality-attribute
- Status: validated
- Description: Opt-in `validate: "full"` mode invokes external type checkers (tsc, pyright, cargo check, go vet) after an edit. Returns type errors with line numbers and messages. Default remains syntax-only (fast).
- Why it matters: Gives agents a one-stop validation option for critical edits — e.g., after multi-file transactions where confirming type safety matters.
- Source: user
- Primary owning slice: M002/S03
- Supporting slices: none
- Validation: S03 — 4 validation integration tests prove validate:"full" through binary protocol, structured ValidationError output (line/column/message/severity), not-found graceful degradation, and lean default responses (validation_errors omitted when not requested). 8 unit tests cover per-checker output parsers (tsc, pyright, cargo, go vet).
- Notes: Synchronous — command blocks until type checker returns. Acceptable because it's opt-in and the agent explicitly requested it.

### R018 — Dry-run mode on all mutations
- Class: failure-visibility
- Status: validated
- Description: Every mutation command accepts `dry_run: true` and returns a diff preview without applying changes. Shows lines added/removed and syntax validity of the proposed change.
- Why it matters: Lets agents preview destructive operations before committing. Essential for cautious multi-file refactors.
- Source: user
- Primary owning slice: M002/S04
- Supporting slices: none
- Validation: S04 — 8 integration tests prove all 12 mutation commands accept `dry_run: true`, return unified diff and `syntax_valid`, leave files unchanged, create no backups. Plugin round-trip test proves dry_run param flows through the plugin bridge. Milestone acceptance: `edit_symbol` dry-run returns diff, file unchanged.
- Notes: Dry-run diff format is unified diff (parseable by agents). Dry-run shows raw edit diff, not post-format result (D071).

### R019 — Multi-file atomic transactions
- Class: core-capability
- Status: validated
- Description: `transaction` applies edits to multiple files atomically — all succeed or all roll back. Each file's result is reported individually. If any file fails validation, all changes revert.
- Why it matters: Multi-file refactors with partial failures leave the codebase in a broken state. Atomic transactions eliminate partial failure.
- Source: user
- Primary owning slice: M002/S04
- Supporting slices: none
- Validation: S04 — 6 integration tests prove transaction success (3-file atomic write), rollback on syntax error (milestone acceptance: 3-file transaction with third failing, all rolled back), new file rollback (deleted on failure), edit_match within transaction, dry-run transaction, empty operations rejection. Plugin round-trip tests prove success and rollback paths through the plugin bridge. Error response includes `failed_operation` index and `rolled_back` array.
- Notes: Builds on the per-file backup system from R007. Supports write and edit_match operations (D072).

### R020 — Call graph construction (lazy, incremental, file watcher)
- Class: core-capability
- Status: validated
- Description: Static call graph built lazily — first query scans outward from the target, caching results. File watcher invalidates/rebuilds graph nodes for changed files. Graph respects worktree boundaries.
- Why it matters: Eager full-workspace scan is too slow for large codebases. Lazy construction with incremental updates gives fast first results and improves over time.
- Source: user
- Primary owning slice: M003/S01
- Supporting slices: M003/S02
- Validation: S01 — Lazy per-file construction with `HashMap<PathBuf, FileCallData>`, cross-file resolution via import chains, worktree-scoped file walking via `ignore` crate. S02 — File watcher using `notify` v8 with drain-at-dispatch pattern, `invalidate_file()` clears stale data, integration tests prove modify-then-query and remove-then-query cycles reflect changes.
- Notes: File watcher runs in the persistent process background. Must exclude node_modules, target/, venv/, etc.

### R021 — Forward call tree
- Class: primary-user-loop
- Status: validated
- Description: `call_tree` expands from an entry point showing all called functions recursively with signatures, body lines, and file locations. Depth-limited with truncation indicators.
- Why it matters: Replaces the file-by-file call tracing workflow (~5000 tokens for 4 files) with a single call (~400 tokens).
- Source: user
- Primary owning slice: M003/S01
- Supporting slices: none
- Validation: S01 — 7 integration tests prove cross-file call tree (main→processData→validate across 3 files), depth limiting, aliased import resolution, unknown symbol errors, not-configured errors. Plugin tool registration verified by bun tests.
- Notes: External calls (third-party libraries) shown as leaf nodes with package name.

### R022 — Reverse caller tree
- Class: primary-user-loop
- Status: validated
- Description: `callers` shows all call sites for a function, grouped by file, with each caller expandable to its own callers up to specified depth.
- Why it matters: "What calls this function?" is a fundamental navigation question that currently requires grep + manual tracing.
- Source: user
- Primary owning slice: M003/S02
- Supporting slices: none
- Validation: S02 — 4 integration tests prove cross-file callers grouped by file, recursive depth expansion with cycle detection, empty result handling (total_callers: 0), not_configured guard, symbol_not_found error. 2 watcher cycle tests prove modify-then-query and remove-then-query reflect changes. Plugin tool registration verified by bun tests.
- Notes: None.

### R023 — Reverse trace to entry points
- Class: differentiator
- Status: validated
- Description: `trace_to` traces backward from a target function to all entry points (route handlers, event listeners, exported functions, main). Renders top-down with data threading — shows how data transforms through the call chain.
- Why it matters: "How does execution reach this function?" — the most powerful single-call navigation primitive. Replaces 5+ file reads.
- Source: user
- Primary owning slice: M003/S03
- Supporting slices: none
- Validation: S03 — 5 integration tests prove backward traversal from deeply-nested utility (checkFormat) to entry points (main), multi-path traces (validate → 2+ entry points), not_configured guard, symbol_not_found error, no-entry-point graceful handling. 5 unit tests prove BFS path collection, cycle detection, depth limiting. Response includes diagnostic fields (total_paths, entry_points_found, max_depth_reached, truncated_paths).
- Notes: Depends on entry point detection heuristics (R026). Framework-specific patterns deferred (D081).

### R024 — Data flow tracking
- Class: differentiator
- Status: validated
- Description: `trace_data` follows a specific expression through the call chain, showing type transformations and variable renames at each hop.
- Why it matters: Understanding how a value flows through code is essential for debugging and security analysis. Currently requires manual tracing across files.
- Source: user
- Primary owning slice: M003/S04
- Supporting slices: none
- Validation: S04 — 5 integration tests prove assignment tracking (variable renames through declarators), cross-file parameter matching (argument position to parameter name via extract_parameters), approximation marking on destructuring/spread, not_configured guard, symbol_not_found error. Response includes depth_limited flag and per-hop approximate markers.
- Notes: Static analysis — tracks through assignments, function parameters, and return values.

### R025 — Change impact analysis
- Class: differentiator
- Status: validated
- Description: `impact` analyzes what breaks if a symbol is changed (e.g., add parameter, change return type). Reports direct callers needing update, indirect callers that may need update, and type impact.
- Why it matters: Agents currently guess at impact or miss callers. Automated impact analysis prevents broken-but-not-caught changes.
- Source: user
- Primary owning slice: M003/S04
- Supporting slices: none
- Validation: S04 — 3 integration tests prove multi-caller impact with signatures, entry point flags, call expressions, and extracted parameters; not_configured guard; symbol_not_found error. 15 unit tests prove extract_parameters across all 6 languages. Response includes total_affected and affected_files counts.
- Notes: Includes suggestions for updating call sites (e.g., "add default argument"). D079 merged trace_data and impact into single slice S04.

### R026 — Entry point detection heuristics
- Class: core-capability
- Status: validated
- Description: Detect entry points heuristically: route handlers (router.get/post, @Get/@Post), event listeners (on/addEventListener), exported functions, main/init/bootstrap functions, test functions (describe/it/test).
- Why it matters: trace_to needs to know where to stop tracing backward. Without entry point detection, traces go to infinity.
- Source: user
- Primary owning slice: M003/S03
- Supporting slices: none
- Validation: S03 — 8 unit tests prove classification of exported functions (not methods), main/init/setup/bootstrap/run patterns (case-insensitive), language-specific test patterns (TS/JS describe/it/test, Python test_/setUp/tearDown, Rust test_, Go Test), and negative cases. Generic patterns shipped; framework-specific patterns (Express, Flask, Axum) deferred per D081.
- Notes: Heuristics are language-specific — generic patterns only for now. Framework-specific patterns added incrementally.

### R027 — Worktree-aware scoping
- Class: quality-attribute
- Status: validated
- Description: All operations respect the project root boundary. Call graph, file watcher, and symbol search do not crawl into node_modules, .git, target/, venv/, or other excluded directories.
- Why it matters: Without scoping, operations on large codebases include irrelevant third-party code, producing noise and performance problems.
- Source: user
- Primary owning slice: M003/S01
- Supporting slices: M001/S01
- Validation: S01 — `walk_project_files()` uses `ignore` crate respecting .gitignore with hardcoded exclusions for node_modules, target, venv, .git. Unit tests prove gitignore exclusion and source-file-only filtering.
- Notes: Should respect .gitignore patterns by default. Configurable exclusion list.

### R028 — Move symbol with import rewiring
- Class: core-capability
- Status: validated
- Description: `move_symbol` moves a function/class/type from one file to another, updates all import statements that reference it across the workspace, and adds necessary exports.
- Why it matters: Manual symbol moves require 5-10 steps (cut, paste, update imports in every consuming file). Single-call operation eliminates the most error-prone refactor.
- Source: user
- Primary owning slice: M004/S01
- Supporting slices: none
- Validation: S01 — 28 Rust tests (19 unit + 9 integration) prove move_symbol across 5+ consumer files including aliased imports, dry-run preview, checkpoint/rollback, and error paths (not_configured, symbol_not_found, non-top-level, file_not_found) through binary protocol. Plugin round-trip verified by bun test (40 tests).
- Notes: Depends on call graph (knows all consumers) and import management (knows how to update imports). Import rewriting scoped to TS/JS/TSX (D110).

### R029 — Extract function
- Class: core-capability
- Status: validated
- Description: `extract_function` takes a line range, identifies free variables that become parameters, determines the return value, extracts the code into a new function, and replaces the original range with a call.
- Why it matters: Extract is the most common refactoring operation. Automating parameter inference and return type detection eliminates the manual analysis step.
- Source: user
- Primary owning slice: M004/S02
- Supporting slices: none
- Validation: S02 — 21 unit tests cover free variable classification (enclosing params, property access filtering, module-level, this/self detection), return value detection (explicit, post-range, void), function generation (TS/Python). 6 integration tests prove end-to-end through binary protocol (basic TS, return value, Python, dry-run, unsupported language, this-reference). Plugin round-trip verified by bun test (42 tests).
- Notes: Limited to TS/JS/TSX and Python per D101. Agent reviews and approves the extraction before it's applied.

### R030 — Inline symbol
- Class: core-capability
- Status: validated
- Description: `inline_symbol` replaces a function call with the function's body, adjusting variable names and scope as needed. Inverse of extract.
- Why it matters: Removes unnecessary indirection — one-line wrapper functions, trivial helpers that obscure the logic.
- Source: user
- Primary owning slice: M004/S02
- Supporting slices: none
- Validation: S02 — 17 unit tests cover parameter substitution (basic, whole-word, noop), single-return validation (single, void, expression body, multiple), scope conflict detection. 6 integration tests prove end-to-end through binary protocol (basic TS, expression body, Python, dry-run, multiple-returns, scope-conflict). Plugin round-trip verified by bun test (42 tests).
- Notes: Must handle scope conflicts when inlining (variable name collisions). Limited to single-return functions per D102.

### R031 — LSP-aware architecture (provider interface)
- Class: constraint
- Status: validated
- Description: Symbol resolution has a provider interface from M001 onward. Tree-sitter is the default provider. Command JSON schema includes optional `lsp_hints` fields. When the plugin has LSP data available, it enriches commands with precise symbol locations, type info, and resolved references.
- Why it matters: Pure tree-sitter resolution is ~80% accurate. LSP data pushes it to ~99%. The architecture must be ready for this upgrade path without refactoring.
- Source: user
- Primary owning slice: M001/S01
- Supporting slices: M004/S03
- Validation: S01 — LanguageProvider trait defined, optional lsp_hints field in RawRequest protocol type. M004/S03 — LspHints struct parsed from req.lsp_hints, apply_lsp_disambiguation wired into 4 command handlers (edit_symbol, zoom, move_symbol, inline_symbol). 13 unit tests + 4 integration tests prove parsing, disambiguation, fallback, and malformed hints handling. Plugin populates lsp_hints via queryLspHints for 5 commands, verified by 13 mock client tests.
- Notes: Full LSP integration complete. Tree-sitter is default; LSP hints enhance disambiguation when plugin provides them.

### R032 — Structured JSON I/O (no shell escaping)
- Class: constraint
- Status: validated
- Description: All communication between plugin and binary uses JSON over stdin/stdout. File content, code snippets, and edit payloads are JSON string values — never shell arguments. Eliminates all shell escaping issues.
- Why it matters: Shell escaping is a persistent source of bugs in agent tools. JSON I/O makes content handling unambiguous.
- Source: user
- Primary owning slice: M001/S01
- Supporting slices: none
- Validation: S01 — all 120 sequential commands and 8 malformed scenarios flow through JSON stdin/stdout, no shell escaping in any path
- Notes: Every request is one JSON object per line. Every response is one JSON object per line.

### R033 — LSP integration via plugin mediation
- Class: integration
- Status: validated
- Description: The TypeScript plugin queries OpenCode's LSP infrastructure and passes enhanced resolution data to the Rust binary as part of command JSON `lsp_hints` fields. Binary uses LSP data when available, falls back to tree-sitter when not.
- Why it matters: Completes the accuracy story — tree-sitter handles structure, LSP provides precise type-level resolution for ambiguous cases.
- Source: user
- Primary owning slice: M004/S03
- Supporting slices: none
- Validation: M004/S03 — Plugin queryLspHints checks lsp.status() → find.symbols() → maps SymbolKind → populates lsp_hints in bridge params for edit_symbol, zoom, move_symbol, inline_symbol, extract_function. Binary parses and applies disambiguation. 13 plugin mock client tests prove connected/disconnected/error paths. 4 binary integration tests prove protocol-level disambiguation and fallback. 55 total bun tests pass.
- Notes: Plugin mediates — binary never connects to language servers directly.

### R034 — Web-first language priority
- Class: constraint
- Status: active
- Description: TypeScript, JavaScript, and TSX/JSX are integrated first (shared tree-sitter query patterns). Python second. Rust and Go third. Each language is fully integrated before moving to the next.
- Why it matters: Most AI agents work in TS/JS. Shipping the most-used languages first maximizes early value.
- Source: user
- Primary owning slice: M001/S02
- Supporting slices: M002/S01
- Validation: unmapped
- Notes: TS/JS/TSX share ~80% of query patterns. Python is structurally different (indent-based scope). Rust and Go each have unique constructs (impl blocks, interface embedding).

## Deferred

### R035 — Multi-language file handling
- Class: core-capability
- Status: deferred
- Description: Handle files with mixed languages (HTML with embedded script/style, Markdown with code blocks).
- Why it matters: Real-world codebases have mixed-language files. Tree-sitter supports language injection but it adds significant complexity.
- Source: research
- Primary owning slice: none
- Supporting slices: none
- Validation: unmapped
- Notes: Deferred because single-language files cover >95% of agent workflows. Can be added in a future milestone.

### R036 — User-extensible compound operation templates
- Class: differentiator
- Status: deferred
- Description: Allow users to define custom compound operations via a template system rather than hardcoding per language.
- Why it matters: Would let the community extend AFT's structural transforms without modifying the binary.
- Source: research
- Primary owning slice: none
- Supporting slices: none
- Validation: unmapped
- Notes: Deferred until the hardcoded compound operations prove the pattern and we understand what users want to extend.

### R037 — Call graph persistence to disk
- Class: quality-attribute
- Status: deferred
- Description: Persist the call graph to disk so it survives binary restarts without full rebuild.
- Why it matters: Large codebases take time to build the call graph. Persistence avoids cold-start penalty.
- Source: research
- Primary owning slice: none
- Supporting slices: none
- Validation: unmapped
- Notes: Deferred because the lazy/incremental strategy with file watcher means the graph rebuilds quickly on demand. Persistence adds state management complexity.

## Out of Scope

### R038 — Replace LSP
- Class: anti-feature
- Status: out-of-scope
- Description: AFT provides fast, approximate analysis via tree-sitter. LSP remains the source of truth for types, diagnostics, and precise resolution.
- Why it matters: Prevents scope creep into building a language server. AFT and LSP are complementary, not competing.
- Source: user
- Primary owning slice: none
- Supporting slices: none
- Validation: n/a
- Notes: AFT can consume LSP data (R033) but does not produce it.

### R039 — Code generation / scaffolding
- Class: anti-feature
- Status: out-of-scope
- Description: AFT manipulates existing code structurally. It does not generate boilerplate, scaffolding, or templates.
- Why it matters: Prevents scope creep. Code generation is a different tool category.
- Source: user
- Primary owning slice: none
- Supporting slices: none
- Validation: n/a
- Notes: Agents handle code generation themselves — AFT helps them place and edit it.

### R040 — GUI / editor integration / watch mode
- Class: anti-feature
- Status: out-of-scope
- Description: AFT is a CLI tool and OpenCode plugin. No GUI, no editor integration, no watch mode.
- Why it matters: Keeps scope focused on the agent use case. Editor plugins are a different product.
- Source: user
- Primary owning slice: none
- Supporting slices: none
- Validation: n/a
- Notes: None.

### R041 — Override built-in Edit/Write tools
- Class: anti-feature
- Status: out-of-scope
- Description: AFT tools are registered alongside built-in Edit/Write, not as replacements. The agent uses whichever is more appropriate.
- Why it matters: Prevents breaking existing workflows. AFT is additive.
- Source: user
- Primary owning slice: none
- Supporting slices: none
- Validation: n/a
- Notes: None.

## Traceability

| ID | Class | Status | Primary owner | Supporting | Proof |
|---|---|---|---|---|---|
| R001 | core-capability | validated | M001/S01 | none | S01 |
| R002 | core-capability | validated | M001/S02 | none | S02 |
| R003 | primary-user-loop | validated | M001/S03 | none | S03 |
| R004 | core-capability | validated | M001/S05 | none | S05 |
| R005 | core-capability | validated | M001/S05 | none | S05 |
| R006 | core-capability | validated | M001/S05 | none | S05 |
| R007 | failure-visibility | validated | M001/S04 | M001/S05 | S04+S05 |
| R008 | failure-visibility | validated | M001/S04 | none | S04 |
| R009 | integration | validated | M001/S06 | none | S06 |
| R010 | quality-attribute | validated | M001/S05 | none | S05 |
| R011 | quality-attribute | validated | M001/S05 | M001/S03 | S05 |
| R012 | launchability | validated | M001/S07 | none | S07 |
| R013 | core-capability | validated | M002/S01 | none | S01 |
| R014 | core-capability | validated | M002/S02 | none | S02 |
| R015 | differentiator | validated | M002/S02 | none | S02 |
| R016 | quality-attribute | validated | M002/S03 | none | S03 |
| R017 | quality-attribute | validated | M002/S03 | none | S03 |
| R018 | failure-visibility | validated | M002/S04 | none | S04 |
| R019 | core-capability | validated | M002/S04 | none | S04 |
| R020 | core-capability | validated | M003/S01 | M003/S02 | S01+S02 |
| R021 | primary-user-loop | validated | M003/S01 | none | S01 |
| R022 | primary-user-loop | validated | M003/S02 | none | S02 |
| R023 | differentiator | validated | M003/S03 | none | S03 |
| R024 | differentiator | validated | M003/S04 | none | S04 |
| R025 | differentiator | validated | M003/S04 | none | S04 |
| R026 | core-capability | validated | M003/S03 | none | S03 |
| R027 | quality-attribute | validated | M003/S01 | M001/S01 | S01 |
| R028 | core-capability | validated | M004/S01 | none | S01 |
| R029 | core-capability | validated | M004/S02 | none | S02 |
| R030 | core-capability | validated | M004/S02 | none | S02 |
| R031 | constraint | validated | M001/S01 | M004/S03 | S01+S03 |
| R032 | constraint | validated | M001/S01 | none | S01 |
| R033 | integration | validated | M004/S03 | none | S03 |
| R034 | constraint | active | M001/S02 | M002/S01 | unmapped |
| R035 | core-capability | deferred | none | none | unmapped |
| R036 | differentiator | deferred | none | none | unmapped |
| R037 | quality-attribute | deferred | none | none | unmapped |
| R038 | anti-feature | out-of-scope | none | none | n/a |
| R039 | anti-feature | out-of-scope | none | none | n/a |
| R040 | anti-feature | out-of-scope | none | none | n/a |
| R041 | anti-feature | out-of-scope | none | none | n/a |

## Coverage Summary

- Active requirements: 1
- Mapped to slices: 34
- Validated: 33
- Unmapped active requirements: 0
