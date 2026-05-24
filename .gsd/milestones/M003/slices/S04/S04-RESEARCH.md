# S04: Data Flow Tracking + Impact Analysis — Research

**Date:** 2026-03-14

## Summary

S04 is a terminal slice adding two leaf commands — `trace_data` and `impact` — that build entirely on the proven call graph infrastructure from S01-S03. No new data structures or infrastructure are needed. Both commands reuse `CallGraph`, the reverse index, `symbol_metadata`, `is_entry_point()`, and the existing command handler pattern.

`impact` is straightforward: it's an enriched `callers_of` that includes the target symbol's signature, groups all transitive callers, annotates each with the call arguments from source, and flags whether callers are entry points. The call sites and signature data are already available. The "suggestion" aspect (per R025) can be simple: compare argument count at call site to parameter count in signature, flag mismatches.

`trace_data` is the harder command. Per D082, it's scoped to tracking through direct assignments and function parameters only — no destructuring, spread, or conditional assignments. The algorithm walks the function body AST for the given expression, tracks assignments (`const x = foo(expr)` → `x` carries the value), and when the expression flows into a call as an argument, follows to the callee's corresponding parameter name by matching argument position to the signature's parameter list. This requires light AST walking per function body — tree-sitter gives us `variable_declarator`, `assignment_expression`, and `arguments` nodes. The key constraint is that tree-sitter doesn't give us type information, so tracking is name-based and approximate.

Both commands follow the exact handler pattern established in S01-S03: param extraction → configure guard → symbol existence check → graph call → serialize. Both get plugin tools in `navigation.ts` following the existing Zod schema pattern. Both get integration tests through the binary protocol using the existing `callgraph/` fixtures.

## Recommendation

**Ship both commands in one slice, impact first, trace_data second.**

Impact is lower risk — it reuses `callers_of()` directly and adds signature-aware annotations. Build it first, prove the handler pattern, then build `trace_data` which requires new AST walking logic for assignment/parameter tracking.

### impact command

Input: `{ file, symbol, depth? }` — find the target symbol's signature, then collect all callers transitively (reuse `callers_of` with depth, default 5), annotate each caller with:
- The call expression text at the call site (extract from source using `CallSite.byte_start/byte_end`)
- Whether the caller is an entry point (reuse `is_entry_point`)
- A simple suggestion: "N arguments passed, M parameters expected" when counts mismatch (for future signature changes), or just "direct caller — update required" for the initial version

Response type: `ImpactResult { symbol, file, signature, total_affected, affected_files, callers: Vec<ImpactCaller> }` where `ImpactCaller` has `{ caller_symbol, caller_file, line, is_entry_point, call_expression }`.

### trace_data command

Input: `{ file, symbol, expression, depth? }` — trace how `expression` flows through calls within `symbol`'s body and across function boundaries.

Algorithm:
1. Parse the function body of `symbol`, find all variable declarations and assignments that reference `expression`
2. For each call where `expression` (or its derivatives) is an argument, resolve the callee and match the argument position to the callee's parameter name
3. In the callee, the parameter name becomes the new tracking target — recurse into the callee's body
4. Track return values: if `const x = foo(expr)`, then `x` carries the return-value flow from `foo`
5. Build a chain of `DataFlowHop` entries: `{ file, symbol, variable, line, flow_type }` where `flow_type` is "parameter", "assignment", or "return"

The AST walking uses tree-sitter nodes: `variable_declarator` (TS/JS), `assignment` (Python), `let_declaration` (Rust) to find assignments. `arguments`/`argument_list` to match argument positions. Signature text parsing to extract parameter names (split on commas, trim types).

Depth-limit cross-file hops (default 5) and mark approximations when tracking breaks (unresolved call, destructuring detected, dynamic access).

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Finding all callers transitively | `CallGraph::callers_of()` with depth parameter | Already proven in S02, handles cycle detection and file grouping |
| Entry point classification per caller | `is_entry_point()` + `symbol_metadata` on `FileCallData` | Proven in S03, covers all 6 languages |
| Cross-file edge resolution | `CallGraph::resolve_cross_file_edge()` | Proven in S01, handles direct, aliased, namespace, and barrel imports |
| Call site extraction from function body | `extract_calls_full()` in `calls.rs` | Returns `(full_callee, short_name, line)` triples — reuse for argument position matching |
| Symbol signature extraction | `SymbolMeta.signature` in `FileCallData` | Already populated from `list_symbols_from_tree` in `build_file_data()` |
| Test harness for binary protocol | `AftProcess` in `tests/integration/helpers.rs` | Established pattern for all integration tests |
| Plugin tool registration | `navigation.ts` Zod schema pattern | 4 existing navigation tools follow identical pattern |

## Existing Code and Patterns

- `src/callgraph.rs` — `CallGraph` struct with `callers_of()`, `forward_tree()`, `trace_to()`, `build_file()`, `build_reverse_index()`, `resolve_cross_file_edge()`, `is_entry_point()`. The `FileCallData` has `calls_by_symbol`, `exported_symbols`, `symbol_metadata`, `import_block`, `lang`. `CallSite` has `callee_name`, `full_callee`, `line`, `byte_start`, `byte_end`. `SymbolMeta` has `kind`, `exported`, `signature`. All the building blocks are here.
- `src/calls.rs` — `extract_calls_full()` returns `(full_callee, short_name, line)` triples from AST walking. `extract_callee_name()` extracts the last segment of member expressions. `extract_full_callee()` returns the full expression. These are the call extraction primitives — `trace_data` can reuse the same AST node walking approach for assignment tracking.
- `src/commands/trace_to.rs` — Handler pattern to follow exactly: param extraction with missing-param errors, `ctx.callgraph().borrow_mut()`, `cg_ref.as_mut()` with `not_configured` guard, symbol existence check via `calls_by_symbol || exported_symbols || symbol_metadata`, graph method call, `serde_json::to_value` serialization.
- `src/commands/callers.rs` — Same handler pattern. Impact handler will be nearly identical structurally but calls a new graph method and returns a richer response type.
- `opencode-plugin-aft/src/tools/navigation.ts` — 4 existing tool definitions (`aft_configure`, `aft_call_tree`, `aft_callers`, `aft_trace_to`). S04 adds `aft_trace_data` and `aft_impact` following the same Zod schema structure.
- `tests/fixtures/callgraph/` — 7 existing fixture files with cross-file import chains. Need to extend fixtures or add new ones for data flow scenarios (variable assignments, argument passing). The existing `utils.ts` with `const valid = validate(input)` already demonstrates the assignment-through-call pattern `trace_data` needs to track.
- `tests/integration/callgraph_test.rs` — 25 existing integration tests through binary protocol. S04 adds tests for `trace_data` and `impact` following the same configure-then-query pattern.
- `src/parser.rs` — `extract_signature()` takes the first line of a function declaration, trims trailing `{`. Signatures look like `export function processData(input: string): string` — parameter names can be parsed from these with simple string splitting.
- `src/symbols.rs` — `Symbol` with `signature: Option<String>`, `SymbolKind`, `Range`. Used indirectly through `SymbolMeta` in the call graph.

## Constraints

- **D082 (trace_data minimal viable scope)** — Track through direct assignments and function parameters only. No destructuring (`const { a, b } = foo()`), spread (`...args`), or conditional assignments (`x = cond ? a : b`). Mark these as "approximate" when detected and stop tracking through them.
- **Signature format is text-based** — Signatures are the first line of the function declaration. Parameter extraction requires text parsing (`(param1: Type, param2: Type)` → `["param1", "param2"]`). This is fragile across languages — Python has `self` as first param, Rust has `&self`/`&mut self`, Go has receiver syntax. Need per-language parameter extraction.
- **Single-threaded RefCell architecture (D001, D014, D029)** — Both commands take `&AppContext` and borrow_mut the callgraph RefCell. Same pattern as existing commands.
- **CallSite stores byte_start/byte_end of the containing symbol, not the call expression itself** — To extract the actual call expression text for impact, we'd need to re-parse and find the specific call node. Alternative: use the `line` field and read the source line at that location, which is simpler and usually sufficient.
- **Impact "suggestions" are necessarily approximate** — Without type information, we can only compare argument count vs parameter count. Specific suggestions like "add default argument" require understanding the change being made, which isn't available statically. Ship with generic "update required" annotations; specific suggestions are a stretch goal.
- **NDJSON protocol (D009)** — Both commands return complete responses, no streaming. Response sizes should stay small since they're summary data (caller lists, data flow chains).

## Common Pitfalls

- **Parameter position off-by-one with self/this** — Python methods have `self` as first parameter, Rust has `&self`/`&mut self`. When matching argument position at a call site (`obj.method(a, b)`) to parameter position in the signature (`def method(self, x, y)`), must skip the receiver parameter. Need per-language receiver detection.
- **Signature parsing edge cases** — Signatures with default values (`fn foo(x: i32 = 5)`), rest parameters (`...args`), destructured parameters (`{ a, b }: Config`), or multiline signatures will break naive comma-splitting. Handle gracefully: if parsing fails, mark as "unresolved parameters" rather than crashing.
- **trace_data cross-file tracking requires matching argument index to parameter name** — If `foo(a, b)` calls `bar(a: number, b: number)`, argument 0 maps to parameter `a`, argument 1 to `b`. The call expression AST provides `arguments` children; the callee signature provides parameter names. Matching is positional. If argument count doesn't match parameter count, stop tracking with an approximation marker.
- **Return value tracking creates bidirectional flow** — `const x = foo(expr)` means `expr` flows INTO `foo` as an argument, but `foo`'s return value flows BACK into `x`. These are different flow directions. For the initial version, track the forward (argument) direction. Return-value tracking is a stretch goal — it requires analyzing the callee's return statements.
- **Impact depth explosion** — A core utility function used everywhere (e.g., `formatString`) could have hundreds of transitive callers. The depth limit from `callers_of` handles this, but the response could still be large. Consider a `max_results` parameter to cap the output.
- **Existing fixture coverage may be thin for data flow** — Current fixtures test cross-file call resolution, not variable tracking within function bodies. Need new fixture files (or extended existing ones) with assignment chains: `const x = foo(input); bar(x);`.

## Open Risks

- **Parameter name extraction reliability** — Signature text is language-specific and not guaranteed to be parseable. Degenerate cases (macros, complex generics, multi-line sigs) will fail. Accept extraction failures gracefully.
- **trace_data usefulness vs complexity tradeoff** — The minimal D082 scope (assignments + parameters only) may be too limited to be useful in real codebases where destructuring and spread are common (especially in TS/JS). Monitor whether the approximation markers dominate the output.
- **Test fixture complexity** — Data flow tests need multi-file fixtures with specific variable patterns. These are more complex to set up than the existing call graph fixtures. May need 2-3 new fixture files.
- **Impact response size** — A heavily-used utility could produce a very large impact response. The depth limit mitigates this, but a `max_results` cap may be needed.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | apollographql/skills@rust-best-practices | available — general Rust patterns, not S04-specific |

No skills directly relevant to static data-flow analysis or impact analysis. The codebase's own patterns are the best guide.

## Sources

- Existing callgraph infrastructure (source: `src/callgraph.rs` — 2257 lines with forward_tree, callers_of, trace_to, reverse index, is_entry_point)
- Call extraction primitives (source: `src/calls.rs` — extract_calls_full, extract_callee_name, walk_for_calls)
- Command handler pattern (source: `src/commands/trace_to.rs`, `src/commands/callers.rs`, `src/commands/call_tree.rs`)
- Plugin tool pattern (source: `opencode-plugin-aft/src/tools/navigation.ts` — 4 existing navigation tool definitions)
- Test fixture structure (source: `tests/fixtures/callgraph/` — 7 fixture files with cross-file imports)
- Integration test harness (source: `tests/integration/helpers.rs` — AftProcess pattern)
- D082 decision: minimal trace_data scope (source: `.gsd/DECISIONS.md`)
- D079 decision: trace_data + impact merged into single slice (source: `.gsd/DECISIONS.md`)
- S03 forward intelligence: trace_to() backward traversal reusable for impact, is_entry_point() reusable, symbol_metadata provides signatures (source: S03-SUMMARY.md)
