# S03: Structural Reading — Research

**Date:** 2026-03-14

## Summary

S03 adds two commands to the aft binary: `outline` (file structure overview) and `zoom` (single symbol body with caller/callee annotations). Both build directly on S02's `TreeSitterProvider`, `FileParser`, and `Symbol` struct.

The outline command is straightforward — transform the flat symbol list from `list_symbols()` into a nested tree structure for the JSON response, grouping methods under their parent classes/structs/impls. The zoom command is more interesting — it needs to extract the symbol's source body with surrounding context lines, plus annotations for outbound calls (what this symbol calls) and inbound callers (what calls this symbol within the same file). Both of these require walking tree-sitter AST nodes within symbol byte ranges to find call expressions.

The primary architectural decision is introducing a `src/commands/` module directory pattern. This is explicitly specified in the boundary map and establishes the convention S04/S05 will follow. Each command module exports a handler function that takes the parsed request params and the provider, returning a `Response`.

## Recommendation

Create `src/commands/mod.rs`, `src/commands/outline.rs`, `src/commands/zoom.rs`. Define serializable response types in each command module. Build outline from the flat symbol list by nesting children under parents via range containment. Build zoom by: (1) resolving the target symbol, (2) extracting its source body + context lines, (3) walking the AST within the symbol's range for `call_expression`/`call`/`macro_invocation` nodes to find calls_out, (4) walking all other symbols' ranges to find calls to this symbol for called_by.

Wire both commands into `main.rs` dispatch. Make `TreeSitterProvider` (currently `_provider`) the active provider passed to command handlers. For zoom's symbol parameter, use `resolve_symbol()` from the `LanguageProvider` trait. When multiple matches return, emit `AmbiguousSymbol` error (R011 support).

Context lines for zoom should default to 3 lines before/after, configurable via optional `context_lines` param.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Flat-to-tree symbol nesting | `Symbol.parent` field + range containment from S02 | Already encoded in extraction — don't re-query the AST for hierarchy |
| Call expression discovery | tree-sitter `call_expression`/`call`/`macro_invocation` node kinds (built into grammars) | Standard node kinds across all grammar crates, no custom queries needed |
| Symbol resolution | `LanguageProvider::resolve_symbol()` | Already handles name matching and error types |
| JSON response envelope | `Response::success()` / `Response::error()` from protocol.rs | Established pattern from S01 |

## Existing Code and Patterns

- `src/parser.rs` — `TreeSitterProvider::list_symbols(path)` returns flat `Vec<Symbol>`. `FileParser::parse(path)` returns `(&Tree, LangId)` for direct AST access. `FileParser::extract_symbols(path)` reads source + parses + extracts. All three are needed by outline and zoom.
- `src/parser.rs` — `node_text(source, &node)` and `node_range(&node)` helpers. Reuse for extracting symbol bodies and call target names.
- `src/language.rs` — `LanguageProvider::resolve_symbol(file, name)` returns `Vec<SymbolMatch>`. When `len() == 1`, we have our target. When `len() > 1`, emit `AmbiguousSymbol` with candidates. This directly supports R011.
- `src/protocol.rs` — `RawRequest` has flattened `params` field. Outline needs `file` param, zoom needs `file` + `symbol` + optional `context_lines`. Extract via `serde_json::from_value`.
- `src/main.rs` — dispatch pattern: `match req.command.as_str() { ... }`. New commands slot in alongside `"ping"`, `"version"`, `"echo"`. `TreeSitterProvider` is already created (currently `_provider`), needs to be passed to handlers.
- `src/error.rs` — `AftError::SymbolNotFound`, `AftError::AmbiguousSymbol` already defined with correct JSON serialization. Zoom uses both.
- `tests/integration/protocol_test.rs` — `AftProcess` struct pattern for integration tests. S03 integration tests follow this exact pattern: spawn binary, send outline/zoom commands, verify JSON responses.

## Constraints

- **Symbol ranges are 0-indexed lines** — tree-sitter `Node.start_position().row` is 0-based. The `Range` struct stores these directly. Source line extraction must account for this.
- **`FileParser` requires `&mut self`** for cache updates — accessed through `RefCell<FileParser>` inside `TreeSitterProvider`. Command handlers receive `&TreeSitterProvider`, calling `parser.borrow_mut()`. Cannot hold the borrow across calls — must be scoped carefully.
- **Caller/callee is file-scoped only** — cross-file call graph is M003 (R020). S03 zoom annotations only cover calls within the same file. This is a deliberate limitation.
- **Call expression node kinds differ by language** — TS/JS/TSX/Go use `call_expression`, Python uses `call`, Rust uses `call_expression` + `macro_invocation`. The `function` field name is consistent for call_expression/call. Rust macros use a `macro` field.
- **Member access in call targets** — `user.getName()` parses as a call with function=`member_expression` text `"user.getName"`. For calls_out, we should extract the final name (`getName`) for matching against symbol names, but include the full text for display.
- **TS type aliases have single-line ranges** — `range: 38..38`. Zoom body extraction for these is trivial (one line) but context_before/context_after still apply.
- **Seven SymbolKind variants** — D019 added TypeAlias. Outline grouping and zoom both handle all 7.
- **`_provider` in main.rs is unused** — needs to become a real dependency of the dispatch function. Handler functions should take `&dyn LanguageProvider` or `&TreeSitterProvider` directly.

## Common Pitfalls

- **RefCell double-borrow panic** — Calling `list_symbols` then `parse` separately on the same `TreeSitterProvider` could panic if the first borrow isn't dropped. Solution: scope borrows explicitly, or use `FileParser` methods that combine parsing and extraction in one borrow.
- **Line counting off-by-one** — Source lines are split with `.lines()` (0-indexed in a Vec), but symbol ranges are also 0-indexed. Context extraction: `source_lines[max(0, start-N)..min(total, end+N+1)]`. The `+1` is easy to miss.
- **Flat-to-tree duplication** — A method that appears in the flat list should appear ONLY as a member of its parent in the outline tree, not also at the top level. Filter by `parent.is_some()` to exclude from top-level.
- **Python nested classes** — `InnerClass` has `scope_chain: ["OuterClass"]` and `inner_method` has `scope_chain: ["OuterClass", "InnerClass"]`. Tree building must handle multi-level nesting, not just one-deep.
- **Rust impl methods appearing under the type, not the impl block** — For outline, methods in `impl MyStruct { ... }` should appear under `MyStruct`. Methods in `impl Drawable for MyStruct { ... }` could appear under `MyStruct` or under a synthetic `impl Drawable for MyStruct` entry. The `parent` field always points to the concrete type, `scope_chain` distinguishes inherent vs trait impl (D017).
- **Zoom symbol disambiguation** — `resolve_symbol("new")` on a Rust file with multiple impl blocks could match methods in different impls. The `AmbiguousSymbol` error includes candidates with scope chains for the agent to disambiguate using qualified names.
- **Empty calls_out / called_by** — Many symbols have no calls (struct definitions, interfaces, enums, type aliases). Return empty arrays, not null/missing fields.

## Open Risks

- **Call extraction accuracy for method calls** — `obj.method()` extracts `"obj.method"` as the function text. Matching this to a symbol named `method` requires stripping the receiver. But the receiver might itself be a method call: `getObj().method()`. For M001, a simple last-segment heuristic is sufficient. Full accuracy requires type resolution (M004/LSP).
- **Macro calls in Rust** — `println!()` is a `macro_invocation`, not a `call_expression`. Including macros in calls_out is informative but can be noisy. Should probably include them with a `"kind": "macro"` annotation.
- **Performance on large files** — Walking the entire AST for every symbol to build called_by could be O(n²) on files with many symbols. For M001 fixture-sized files this is negligible. If large files prove slow, we can pre-build a call-site index in a single pass.
- **Context lines at file boundaries** — Zoom on the first function in a file: context_before has 0 lines. Zoom on the last function: context_after has 0 lines. Both are valid edge cases that need handling.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | plurigrid/asi@tree-sitter | available (7 installs — low relevance, general AST skill, not Rust-specific) |
| tree-sitter | ssiumha/dots@tree-sitter | available (3 installs — too low to be useful) |
| Rust | n/a | no relevant agent skills for Rust binary development |

No skills worth installing — this is specialized Rust + tree-sitter work where the codebase patterns from S01/S02 are the primary reference.

## Sources

- Call expression node kinds verified by exploratory tests against TS, Python, Rust, Go grammars in the project's tree-sitter crates (source: local testing)
- Symbol nesting hierarchy verified by dumping flat symbol lists for all fixtures — parent/scope_chain fields correctly encode the tree structure (source: local testing)
- S02 summary forward intelligence on TreeSitterProvider API, RefCell pattern, range semantics (source: `.gsd/milestones/M001/slices/S02/S02-SUMMARY.md`)
