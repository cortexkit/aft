# S02: Extract Function & Inline Symbol — Research

**Date:** 2026-03-14

## Summary

S02 delivers two single-file refactoring commands: `extract_function` (R029) and `inline_symbol` (R030). Both are self-contained within a single file — no multi-file coordination like move_symbol. The codebase is well-prepared: tree-sitter AST walking patterns from `callgraph.rs` (variable declaration detection, assignment tracking, scope walking) directly inform free variable detection for extract_function. The edit pipeline (`write_format_validate`, `auto_backup`, `dry_run_diff`) and the command handler pattern (`handle_*(req, ctx)`) are battle-tested across 26 existing commands. The plugin tool registration pattern in `refactoring.ts` already has a comment placeholder for S02 additions.

The primary complexity is **free variable classification** for extract_function: walking the selected line range's AST to identify which identifiers are referenced but not declared within that range, then classifying them as parameters (declared in enclosing function scope) vs module-level bindings (don't become parameters) vs `this`/`self` references (require method extraction or error). The callgraph's `walk_for_data_flow` and `extract_assignment_info` already handle the same node kinds (`variable_declarator`, `assignment_expression`, `let_declaration`) — the free variable detector uses the same AST vocabulary but inverts the question: instead of "where does this variable flow?" it asks "where was this variable declared?"

For inline_symbol, the key constraint is D102 (single-return only). This dramatically simplifies the problem: find the function, extract its body, map arguments to parameters, check for variable name conflicts at the call site, and replace the call expression with the body. Scope conflict detection reuses the same declaration-walking pattern needed for extract_function.

Language scope is TS/JS/TSX and Python per D101. Rust and Go are deferred due to ownership/lifetime inference (Rust) and multiple return values (Go).

## Recommendation

Build extract_function first — it's the more complex operation and its free variable detection utility is reused by inline_symbol's scope conflict detection. Structure as:

1. **Free variable detection module** (`src/extract.rs` or similar) — shared utility for walking a byte range, collecting identifier references and declarations, classifying variables
2. **`extract_function` command handler** — uses free variable detection, generates new function + call site replacement
3. **`inline_symbol` command handler** — uses scope conflict detection (subset of free variable analysis), handles argument-to-parameter substitution
4. **Plugin tools** — add `aft_extract_function` and `aft_inline_symbol` to `refactoring.ts`

Both commands follow the canonical mutation pattern from `edit_symbol.rs`: validate params → resolve context → compute edit → check dry_run → auto_backup → write_format_validate → build response.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Finding a symbol's definition and range | `ctx.provider().resolve_symbol()` + `Symbol.range` | Already handles all 6 languages, disambiguation, scope chains. Inline needs this to find the function to inline. |
| Computing byte offsets from line/col | `edit::line_col_to_byte()` | Used by every mutation command. Range → byte offset is the bridge between symbol resolution and text manipulation. |
| Pre-mutation backup | `edit::auto_backup()` | Per-file undo. Every mutation command calls this. |
| Write + format + validate tail | `edit::write_format_validate()` | D046/D066 mandate this for all mutations. Handles format detection, syntax validation, type checker invocation. |
| Dry-run diff generation | `edit::dry_run_diff()` | D071 — unified diff preview. Already integrated via `similar` crate. |
| AST parsing for a file | `grammar_for()` + `Parser::new()` + `parser.parse()` | Tree-sitter setup. Used directly in callgraph.rs, zoom.rs, and trace_data. Extract/inline need raw AST access, not just symbol-level. |
| Language detection | `detect_language()` | File extension → LangId. Guards for D101 language subset. |
| Indentation detection | `indent::detect_indent()` | Extracted function must match the file's indent style. Already a shared utility. |
| Variable declaration node kinds | Pattern from `callgraph.rs::extract_assignment_info()` | Same node kinds: `variable_declarator`, `assignment_expression`, `let_declaration`, etc. |
| Call expression extraction | `calls.rs::extract_calls_in_range()` | For inline: finding call sites of the target function within a file. |

## Existing Code and Patterns

### Direct Reuse

- `src/edit.rs` — `line_col_to_byte()`, `auto_backup()`, `write_format_validate()`, `dry_run_diff()`, `is_dry_run()`, `validate_syntax_str()`. The entire mutation tail. Extract and inline follow the same write path as every other mutation command.
- `src/parser.rs` — `detect_language()`, `grammar_for()`, `node_text()`, `node_range()`, `FileParser::extract_symbols()`. Raw AST access for free variable walking. `node_text` and `node_range` are `pub(crate)` (D057).
- `src/calls.rs` — `extract_calls_in_range()`, `extract_callee_name()`. For inline: finding where the target function is called within the file being modified.
- `src/callgraph.rs` — `extract_assignment_info()` pattern (not the function itself — it's a method on CallGraph). The node-kind matching for declarations (`variable_declarator`, `assignment_expression`, `assignment`, `let_declaration`, `short_var_declaration`) is the vocabulary for free variable detection.
- `src/indent.rs` — `detect_indent()` for matching the file's indentation style in generated function declarations.
- `src/symbols.rs` — `Symbol`, `SymbolKind`, `Range` types.
- `src/protocol.rs` — `RawRequest`, `Response` for command interface.
- `src/context.rs` — `AppContext` with `provider()`, `backup()`, `config()`.

### Pattern to Follow

- `src/commands/edit_symbol.rs` — Canonical mutation command: validate params → resolve symbol → read file → compute edit → dry_run check → auto_backup → write_format_validate → response. Both new commands follow this shape exactly.
- `src/commands/move_symbol.rs` — Rollback-on-failure pattern (though extract/inline are single-file, so simpler). Also shows how to use `line_col_to_byte` for text extraction from symbol ranges.
- `opencode-plugin-aft/src/tools/refactoring.ts` — Plugin tool registration. S02 adds `aft_extract_function` and `aft_inline_symbol` to the return object of `refactoringTools()`. Comment in file already says "S02 will extend this."

### Pattern to Avoid

- `src/commands/transaction.rs` — Multi-file atomic coordination. Extract and inline are single-file operations; don't over-engineer with transaction infrastructure.

## Constraints

- **Language subset: TS/JS/TSX and Python only** (D101). Rust has ownership/lifetime inference complexity; Go has multiple return values. Guard with `detect_language()` and return `unsupported_language` error for Rust/Go.
- **Single-return only for inline** (D102). Functions with multiple return statements (early returns, conditional returns) are rejected. Only inline functions with a single `return` statement, an implicit return (expression body arrow function), or no return (void/statement body).
- **Scope conflicts reported, not auto-resolved** (D103). When inlining would create variable name collisions, return a structured error with the conflicting names and suggested alternatives. Don't auto-rename.
- **Auto-format + validate on every mutation** (D046, D066). Both commands must call `write_format_validate()`.
- **Dry-run on all mutations** (D071). Both commands must support `dry_run: true`.
- **Export keyword not in symbol range** (D030). When extract_function reads the enclosing function's body, the symbol range from `resolve_symbol` doesn't include `export`. This is fine — we're reading within the body, not replacing the declaration.
- **Handler signature** (D026). `handle_extract_function(req: &RawRequest, ctx: &AppContext) -> Response`.
- **Binary is single-threaded with RefCell** (D001, D014, D029). No new stores needed for extract/inline — they're stateless operations that read, compute, and write.

## Common Pitfalls

- **Free variable false positives from property access** — `obj.method()` contains identifier `obj` which IS a free variable, but `obj.property` where `property` is a property_identifier node is NOT a free variable reference to `property`. Must distinguish `identifier` nodes (variable references) from `property_identifier`/`field_identifier` nodes (property access). Tree-sitter makes this distinction in node kinds.

- **Module-level vs function-level declarations** — A variable declared at module scope (`const API_URL = "..."`) that's referenced in the extracted range should NOT become a parameter — it's available everywhere. Must walk upward from the extracted range to find the enclosing function scope, then classify: declared in enclosing function body → parameter; declared at module scope → skip; declared in extracted range → local (not a parameter). Tree-sitter scope boundary nodes: `function_declaration`, `arrow_function`, `method_definition` (TS/JS), `function_definition` (Python), `program` (module root).

- **`this`/`self` references in extracted range** — If the range contains `this` (TS/JS) or `self` (Python), the extracted function can't be a standalone function — it needs to be a method on the same class, or the agent needs to be informed. Safest approach: detect `this`/`self` references and return a structured error recommending method extraction (which is out of scope per D100 top-level only). Alternative: pass `this`/`self` as a parameter with a warning.

- **Return value detection** — Three cases: (a) extracted range ends with a `return` statement → return value is that expression, (b) extracted range assigns to a variable used after the range → that variable is the return value, (c) neither → void function. Case (b) requires scanning the code AFTER the extracted range within the enclosing function for references to variables declared in the range.

- **Inline: expression statements vs return expressions** — When inlining `function add(a, b) { return a + b; }` at call site `const x = add(1, 2)`, the result should be `const x = 1 + 2` (expression substitution). But when the call is a standalone statement `add(1, 2)`, the inlined body minus the `return` keyword is the replacement. Must detect whether the call is the RHS of an assignment or a standalone expression statement.

- **Inline: argument-to-parameter name collision** — When `function foo(x) { return x + 1 }` is called as `foo(x)` where the argument happens to be named `x`, no substitution is needed in the body. But `foo(y)` requires substituting all `x` references in the body with `y`. Must build a parameter→argument mapping and do text substitution carefully (whole-word only, not inside strings/comments).

- **Python indentation as scope delimiter** — Python functions don't have braces. The extracted function body must be de-indented to the new function's level, and the call site must match the original indentation. `detect_indent()` helps, but Python requires more careful indent management than brace languages.

- **Line range to byte range conversion** — extract_function takes a line range (start_line, end_line). Must convert to byte range for AST node containment checks. Use `line_col_to_byte(source, start_line, 0)` for start and `line_col_to_byte(source, end_line + 1, 0)` for end (or end of the last line).

## Open Risks

- **Free variable detection accuracy across edge cases** — Destructured parameters (`const { a, b } = obj`), rest parameters (`...args`), computed property access (`obj[key]`), and template literal expressions all contain identifiers that need careful classification. Starting with the common patterns (simple identifiers, dot access) and marking exotic patterns as approximate (following D082 trace_data precedent) is pragmatic.

- **Python scope rules differ from JS** — Python has LEGB scope (Local, Enclosing, Global, Built-in). A variable used in the extracted range might be from an enclosing function scope (closure variable), not the immediate enclosing function. This is rare but possible. Treat all non-local, non-module-level references as parameters for safety.

- **Inline substitution correctness** — Text-based argument→parameter substitution can go wrong with identifiers that are substrings of other identifiers (e.g., parameter `i` appearing in variable `items`). Must use whole-word boundary matching. Tree-sitter-based substitution (find all `identifier` nodes matching the parameter name) is more reliable than regex.

- **Performance is not a concern** — Both operations are single-file, single-parse. Tree-sitter parsing is ~1ms. No workspace scanning needed. No risk of cold-start penalties.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | `plurigrid/asi@tree-sitter` | available (7 installs — too low adoption, not relevant to this domain-specific work) |
| Rust | `apollographql/skills@rust-best-practices` | available (2.4K installs — general patterns, not specific to AST refactoring) |
| Refactoring | `skillcreatorai/ai-agent-skills@code-refactoring` | available (527 installs — generic refactoring guidance, not AST engine implementation) |

No directly relevant skills. The codebase's own patterns (26 command handlers, AST walking in callgraph.rs, edit pipeline) are the primary guide.

## Sources

- `src/callgraph.rs` — `walk_for_data_flow()` (line 1362) and `extract_assignment_info()` (line 1464) provide the declaration-detection node kind vocabulary: `variable_declarator`, `assignment_expression`, `augmented_assignment_expression`, `assignment`, `let_declaration`, `short_var_declaration`
- `src/calls.rs` — Call extraction utilities (`extract_calls_in_range`, `extract_callee_name`, `extract_full_callee`) for finding call sites within a byte range
- `src/commands/move_symbol.rs` — 1066-line reference for multi-file mutation with checkpoint/rollback, dry-run, and the `line_col_to_byte` pattern for symbol text extraction
- `src/commands/edit_symbol.rs` — Canonical single-file mutation handler pattern (validate → resolve → compute → dry_run → backup → write_format_validate → respond)
- `src/edit.rs` — Shared mutation infrastructure: `write_format_validate()` (line 163), `auto_backup()`, `dry_run_diff()`, `is_dry_run()`, `line_col_to_byte()`
- `src/parser.rs` — Tree-sitter query patterns for all 6 languages, `detect_language()`, `grammar_for()`, `node_text()`, `node_range()` (pub(crate))
- `opencode-plugin-aft/src/tools/refactoring.ts` — Plugin tool registration with explicit "S02 will extend this" comment
- Test baseline: 242 unit + 154 integration (Rust), 40 plugin tests (bun). S02 target: add ~20-30 unit tests (free variable detection, scope conflict detection, parameter extraction) + ~15-20 integration tests (extract/inline success paths, dry-run, error paths across TS/JS/Python)
