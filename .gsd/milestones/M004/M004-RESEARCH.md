# M004: Refactoring Primitives — Research

**Date:** 2026-03-14

## Summary

M004 adds three refactoring commands (`move_symbol`, `extract_function`, `inline_symbol`) and LSP integration via the plugin. The codebase is well-prepared — M001-M003 built exactly the primitives these operations compose: tree-sitter symbol resolution, import management, call graph with reverse index, and the edit pipeline with backup/format/validate. The architecture decisions (persistent binary, AppContext dispatch, RefCell stores, NDJSON protocol, plugin-as-bridge) are stable and extend cleanly.

The primary risk isn't any single operation in isolation — it's that `move_symbol` is a multi-file coordination problem that strings together symbol extraction, file writing, import removal in the source, import addition in the destination, and import path rewriting across every consumer file. The call graph's `callers_of` gives the consumer list, and the import engine's `add_import`/`remove_import` do the rewrites, but the orchestration has many edge cases (barrel re-exports, default vs named imports, the moved symbol being a re-export itself). `extract_function` and `inline_symbol` are single-file operations that are complex but self-contained.

The LSP integration story has a concrete discovery: OpenCode's SDK exposes `client.find.symbols()` and `client.find.text()` — these are LSP-backed workspace symbol search and text search. The plugin can use these to resolve ambiguous symbols and verify references. However, the SDK does **not** expose go-to-definition, find-references, or type information directly. LSP integration will be narrower than the context anticipated — it's "workspace symbol verification" rather than full LSP protocol access. This is still valuable (disambiguating symbols that tree-sitter can't) but the accuracy improvement will be modest rather than the "80% → 99%" claimed in the context. The `lsp_hints` field in `RawRequest` is already wired (D003) — we just need to define what data flows through it.

## Recommendation

**Start with `move_symbol`** — it's the highest-value, highest-risk operation and exercises the most integration points (call graph, import engine, edit pipeline, multi-file transactions). Proving it works validates the composition story for the entire milestone. Follow with `extract_function` + `inline_symbol` (lower risk, single-file, can be done in one slice). LSP integration last — it enhances accuracy of existing operations, and we need the refactoring commands working first to understand exactly which LSP data is most useful.

Slice ordering should be: S01 move_symbol, S02 extract+inline, S03 LSP integration. This front-loads the integration risk and lets us ship the three core refactoring tools even if LSP integration proves narrower than expected.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Finding all files that import a symbol | `CallGraph::callers_of()` + `CallGraph::build_reverse_index()` | Already scans workspace, resolves import chains, handles aliases and re-exports. Building a separate reference finder would duplicate 500+ lines of graph code. |
| Adding imports to consumer files | `imports::find_insertion_point()` + `imports::generate_import_line()` + `imports::is_duplicate()` | The import engine handles 6 languages, group classification, alphabetization, deduplication. Re-implementing any of this would introduce inconsistencies with existing import commands. |
| Removing imports from the source file | `remove_import` command handler pattern | Already handles partial removal (one name from a multi-name import) and full statement removal. |
| Free variable detection for extract_function | tree-sitter AST walking (pattern from `walk_for_data_flow` in callgraph.rs) | The callgraph already walks function bodies identifying variable references and assignments. Same AST traversal pattern works for free variable detection. |
| Unified diff generation | `similar` crate via `edit::dry_run_diff()` | Already integrated (D044), used by all dry-run paths. |
| Multi-file atomicity | Transaction infrastructure from M002/S04 | Transaction with rollback already handles multi-file atomic writes with syntax validation. `move_symbol` is essentially a specialized transaction. |
| Scope conflict detection for inline | tree-sitter variable declaration walking | Same AST node kinds (`variable_declarator`, `let_declaration`, etc.) already handled in `extract_assignment_info()` in callgraph.rs. |

## Existing Code and Patterns

### Core Infrastructure (Reuse Directly)

- `src/callgraph.rs` — `callers_of()` returns all callers of a symbol across the workspace. `build_file_data()` extracts exported symbols, imports, and call sites per file. `resolve_cross_file_edge()` follows import chains to resolve callee locations. The reverse index is the key data structure for move_symbol's consumer discovery.
- `src/imports.rs` — `parse_imports()`, `find_insertion_point()`, `generate_import_line()`, `is_duplicate()`, `classify_group()`. These are the building blocks for rewriting imports after a move. The `ImportBlock`/`ImportStatement` types carry all the import metadata needed.
- `src/edit.rs` — `write_format_validate()` is the shared mutation tail (write → format → validate). `auto_backup()` for pre-mutation snapshots. `dry_run_diff()` for preview. `validate_syntax_str()` for in-memory validation.
- `src/commands/transaction.rs` — Transaction pattern for multi-file atomic operations. `move_symbol` should reuse or extend this for atomic multi-file rewrites.
- `src/parser.rs` — `TreeSitterProvider`, `FileParser`, `extract_symbols()`, `detect_language()`, `grammar_for()`. Symbol extraction with full range/signature/scope_chain data.
- `src/context.rs` — `AppContext` with all RefCell-wrapped stores. New commands follow the `handle_*(req, ctx)` pattern (D026).

### Patterns to Follow

- `src/commands/edit_symbol.rs` — Canonical mutation command pattern: validate params → resolve symbol → read file → compute edit → check dry_run → auto_backup → write_format_validate → build response. Every new command should follow this shape.
- `src/commands/add_import.rs` — Import mutation pattern: detect language → parse existing imports → check duplicates → find insertion point → generate import line → apply edit.
- `src/commands/callers.rs` — Graph command pattern: check callgraph configured → borrow_mut graph → execute query → convert result to JSON response.
- `opencode-plugin-aft/src/tools/navigation.ts` — Plugin tool registration pattern: Zod schema, bridge.send(), JSON.stringify(response). New tools follow this exact shape.
- `opencode-plugin-aft/src/bridge.ts` — `BinaryBridge.send()` is the only interface between plugin and binary. New data (like LSP hints) flows through the existing `params` JSON.

### Integration Points for LSP

- `src/protocol.rs` — `RawRequest.lsp_hints: Option<serde_json::Value>` already exists (R031/D003). This is where LSP data arrives in the binary.
- `opencode-plugin-aft/node_modules/@opencode-ai/sdk` — `client.find.symbols({ query })` returns LSP workspace symbols with `{ name, kind, location: { uri, range } }`. `client.find.text({ pattern })` returns text search results. `client.lsp.status()` returns connected server status.
- `src/language.rs` — `LanguageProvider` trait has only `resolve_symbol()` and `list_symbols()`. LSP-enhanced resolution would add an alternative implementation or enhance the existing one with fallback behavior.

## Constraints

- **Binary never connects to language servers directly** (D002, hard constraint). All LSP data flows through the plugin. Binary receives JSON; plugin queries OpenCode SDK.
- **Single-threaded binary with RefCell** (D001, D014, D029). All new stores/data must follow the RefCell interior mutability pattern. No Mutex, no threads.
- **NDJSON protocol** (D009). New commands are one JSON object per line in, one JSON object per line out. No streaming, no multi-response.
- **Auto-format + validate on every mutation** (D046, D066). All new mutation commands must call `write_format_validate()` as their tail. No exceptions.
- **Transaction limited to write + edit_match** (D072). `move_symbol` can't use the existing transaction command directly — it needs edit_symbol-level operations. Will need its own multi-file coordination, possibly extending the transaction infrastructure.
- **OpenCode SDK LSP surface is limited**. Only `find.symbols()` (workspace symbol search), `find.text()` (text search), and `lsp.status()` (server status). No go-to-definition, find-references, or type information endpoints exposed to plugins. LSP integration will be workspace-symbol-level, not full-protocol.
- **Import management targets top-level only** (D041). Move symbol import rewiring inherits this limitation — won't update conditional/nested imports.
- **Export keyword not included in edit_symbol ranges** (D030). When extracting a symbol for move, the export keyword is separate from the declaration range.

## Common Pitfalls

- **Barrel file re-exports break naive import rewriting** — If `index.ts` re-exports `{ foo }` from `./utils`, and consumers import from the barrel file, moving `foo` to `./helpers` requires updating both the barrel re-export AND deciding whether consumers should now import from `./helpers` directly. The callgraph already handles barrel resolution (`find_index_file`, `file_exports_symbol`), but the write-back logic is new and untested. Avoid by starting with direct imports only, extending to barrel files as a follow-on.

- **Default vs named export confusion during move** — A symbol might be exported as default in the source file but imported as a named import via barrel re-export. Moving it changes the export semantics. Avoid by preserving the original export style in the destination file and updating import statements to match.

- **Relative import path computation** — When updating imports in consumer files, the new import path must be relative from the consumer's directory to the destination file, not from the project root. Getting this wrong produces `import { foo } from '../../../wrong/path'`. Use `std::path::Path::strip_prefix` and careful parent walking.

- **extract_function: closures and `this`/`self` references** — Free variable detection must distinguish between: (a) local variables that become parameters, (b) module-level variables/constants that don't need parameters, (c) `this`/`self` references that mean the extracted function must be a method, not a standalone function. Tree-sitter can identify these through scope chain analysis — variables declared in an outer function body are parameters, variables declared at module scope are not.

- **inline_symbol: variable name collisions** — When inlining `function foo(x) { return x + 1 }` into a scope that already has `x` defined, the inlined body's `x` (which maps to the call argument) collides. The safe approach is to detect conflicts and report them rather than auto-rename (D: recommend reporting with suggestions, per context open question). Auto-renaming is risky because it can change semantics in subtle ways.

- **inline_symbol: multiple return statements** — A function with early returns can't be trivially inlined by replacing the call with the body. Need to either reject (safest) or transform returns into variable assignments + conditional flow. Start with single-return-only restriction.

- **move_symbol: the moved symbol's own imports** — When moving `function foo()` from `a.ts` to `b.ts`, `foo` might import things from `a.ts`'s perspective (e.g., `import { bar } from './bar'`). In `b.ts`, the relative path to `bar` may be different. Must analyze which imports in the source file are used by the moved symbol and rewrite their paths for the destination.

## Open Risks

- **OpenCode LSP API stability** — The `client.find.symbols()` API is part of the SDK but not documented for plugin use in AFT's context. If OpenCode changes the SDK or the symbol data format, the LSP integration slice breaks. Mitigate by keeping the LSP path optional (tree-sitter fallback always works) and testing against the current SDK version.

- **Move symbol import rewriting completeness** — The reverse index from `callers_of` may miss consumers that reference the symbol through re-exports, dynamic imports, or `require()` calls. The callgraph currently handles static import chains only. Accept this limitation and document it clearly in the tool description.

- **Extract function across language idioms** — Each language has different function declaration syntax, parameter/return type annotation conventions, and scope rules. Extract function for 6 languages is a large surface area. Consider shipping for TS/JS/Python first (web-first priority, D004) and extending to Rust/Go as follow-on.

- **Performance of move_symbol on large codebases** — `callers_of` scans all project files to build the reverse index. For a 10K+ file project, this could take seconds. The lazy caching helps on subsequent calls but the first move in a session hits the cold-start penalty. Not a blocker but worth noting in tool documentation.

- **Multi-file backup/undo for move_symbol** — Current undo is per-file. A move_symbol operation modifies N+2 files (source, destination, N consumers). Undoing requires reverting all of them. Checkpoint is the right mechanism (R008) but the move command should auto-create a named checkpoint before executing.

## Candidate Requirements (Advisory — Not Auto-Binding)

These observations from research could become requirements but are **not automatically in scope**. Surface for planning discussion:

1. **Auto-checkpoint before multi-file refactors** — move_symbol should auto-create a named checkpoint before executing, so the entire operation can be undone with `restore_checkpoint`. This is behavioral (how to use existing R008) rather than a new capability.

2. **Extract function language subset** — Consider scoping extract_function to TS/JS/TSX/Python initially (4 of 6 languages) and extending to Rust/Go in a follow-on. Rust has unique extraction patterns (lifetimes, ownership) and Go has multiple return values. Both significantly increase complexity.

3. **Inline symbol single-return restriction** — Restricting inline_symbol to single-return functions (or functions with only a tail return) eliminates the most dangerous edge cases. Multi-return inline can be added later if agents request it.

4. **Move symbol top-level only** — Per the context's open question, restrict move_symbol to top-level symbols (functions, classes, types). Moving individual methods out of a class is significantly more complex and a separate capability.

5. **LSP integration scope narrower than originally planned** — R033 describes "LSP integration via plugin mediation" — the actual OpenCode SDK surface is workspace symbol search only, not full LSP protocol access. The `lsp_hints` mechanism works but the data available is limited to symbol name/kind/location. Consider reframing R033 as "workspace symbol verification" rather than full LSP integration. Still valuable for disambiguation.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | `plurigrid/asi@tree-sitter` | available (7 installs — low adoption, likely not worth installing) |
| Rust | `apollographql/skills@rust-best-practices` | available (2.4K installs — general patterns, not specific to this work) |

No directly relevant professional skills found. The work is domain-specific (AST-based refactoring engine) and the existing codebase patterns are the primary guide.

## Sources

- OpenCode plugin SDK types examined directly from `node_modules/@opencode-ai/sdk/dist/gen/types.gen.d.ts` — found `FindSymbolsData`, `LspStatus`, `Range` types. Plugin has access to `client.find.symbols()` for LSP workspace symbol search.
- OpenCode plugin API examined from `node_modules/@opencode-ai/plugin/dist/index.d.ts` — `PluginInput` provides `client`, `project`, `directory`, `worktree`, `serverUrl`, `$` (BunShell). No direct LSP protocol access.
- Codebase analysis: 145 integration tests, ~16K lines of Rust source across 40 files, ~2K lines of TypeScript plugin code. All patterns well-established across M001-M003.
- Decision register (99 decisions D001-D099) reviewed for constraints affecting M004 implementation.
