---
estimated_steps: 7
estimated_files: 6
---

# T02: Build inline_symbol command with scope conflict detection

**Slice:** S02 — Extract Function & Inline Symbol
**Milestone:** M004

## Description

Build the `inline_symbol` command handler that replaces a function call with the function's body, performing argument-to-parameter substitution and scope conflict detection. Reuses `extract.rs` utilities from T01 for scope analysis. The single-return constraint (D102) keeps the implementation focused — no control flow transformation needed.

## Steps

1. Add `detect_scope_conflicts(source, tree, insertion_byte, param_names, body_text, lang)` to `src/extract.rs` — at the call site's scope level, collect all declared variable names; check for collisions with variables declared in the function body being inlined. Return `Vec<ScopeConflict>` with conflicting name + suggested alternative (append `_inlined` or similar).
2. Add `validate_single_return(source, tree, fn_node, lang)` to `src/extract.rs` — count `return_statement` nodes in the function body. Arrow functions with expression bodies (no `return` keyword) count as single-return. Functions with 0 returns (void) are valid. Functions with >1 return → reject.
3. Add `substitute_params(body_text, param_to_arg_map)` to `src/extract.rs` — replace parameter name identifiers in the body text with corresponding argument expressions. Use whole-word boundary matching to avoid replacing substrings (e.g., parameter `i` inside identifier `items`). Tree-sitter-based: parse the body snippet, find `identifier` nodes matching parameter names, replace from end to start.
4. Create `src/commands/inline_symbol.rs` with `handle_inline_symbol(req, ctx)`: validate params (file, symbol, call_site_line) → read file → detect_language guard (D101) → resolve function symbol via `ctx.provider()` → validate single-return (D102) → extract function body text → find call expression at `call_site_line` using `extract_calls_in_range` → determine call context (assignment RHS vs standalone expression) → build param→arg map from call arguments → check scope conflicts (D103) → if conflicts, return structured error → substitute params in body → adjust indentation → replace call expression with body → dry_run check → auto_backup → write_format_validate → response.
5. Add `pub mod inline_symbol;` to `src/commands/mod.rs`, dispatch entry `"inline_symbol"` to `src/main.rs`.
6. Create test fixtures in `tests/fixtures/inline_symbol/` — at minimum: `sample.ts` (simple helper function + call site), `sample_multi.ts` (function with multiple returns for rejection), `sample_conflict.ts` (function whose body declares a variable that collides with call site scope), `sample.py` (Python equivalent).
7. Write unit tests: `validate_single_return` (single return, expression body, no return, multiple returns), `detect_scope_conflicts` (no conflict, collision detected, suggested rename), `substitute_params` (basic substitution, whole-word boundary, no-op when arg matches param name), handler param validation, unsupported language rejection. Run `cargo test inline_symbol` and `cargo test`.

## Must-Haves

- [ ] Single-return validation rejects functions with >1 return statement (D102)
- [ ] Expression-body arrow functions treated as valid single-return
- [ ] Argument-to-parameter substitution uses whole-word matching (no false replacements)
- [ ] Scope conflict detection finds variable name collisions at call site and returns structured `scope_conflict` error with `conflicting_names` and `suggestions` (D103)
- [ ] Call site context detected: assignment RHS (`const x = fn()`) vs standalone (`fn()`) — correct replacement shape for each
- [ ] dry_run mode returns diff without modifying file
- [ ] auto_backup called before mutation
- [ ] write_format_validate called for the mutation
- [ ] Unsupported language returns `unsupported_language` error code
- [ ] Multiple-returns returns `multiple_returns` error code

## Verification

- `cargo test inline_symbol` — all unit tests pass
- `cargo test` — full suite passes, zero regressions
- Manual spot-check: send an `inline_symbol` command via the binary for a TS fixture and verify the function call is replaced with the body

## Observability Impact

- **Runtime signal:** stderr log `[aft] inline_symbol: {symbol} at {file}:{line}` on successful inlining
- **Inspection surface:** response JSON includes `conflicts` array (empty on success), `call_context` ("assignment" | "standalone"), `substitutions` count for agent introspection
- **Failure visibility:** structured error codes — `unsupported_language` (D101), `multiple_returns` (D102) with `return_count`, `scope_conflict` (D103) with `conflicting_names` and `suggestions` arrays
- **Dry-run:** `dry_run: true` returns unified diff + `syntax_valid` without mutation — agent can preview before committing
- **Future agent debugging:** error messages include symbol name, file path, and line number for grep-friendly diagnostics

## Inputs

- `src/extract.rs` — free variable detection utilities from T01 (scope walking patterns reused for conflict detection)
- `src/calls.rs` — `extract_calls_in_range` for finding call sites
- `src/edit.rs` — `line_col_to_byte`, `auto_backup`, `write_format_validate`, `dry_run_diff`, `is_dry_run`
- `src/parser.rs` — `detect_language`, `grammar_for`, `node_text`, `node_range`
- `src/commands/extract_function.rs` — handler pattern established in T01

## Expected Output

- `src/commands/inline_symbol.rs` — command handler with unit tests (~250-350 lines)
- `src/extract.rs` — extended with `detect_scope_conflicts`, `validate_single_return`, `substitute_params` + their unit tests
- `tests/fixtures/inline_symbol/` — 4+ fixture files
- Dispatch wired in main.rs, module registered in commands/mod.rs
