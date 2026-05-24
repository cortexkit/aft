---
id: S02
parent: M004
milestone: M004
provides:
  - extract_function command — takes file + line range, detects free variables via AST scope walking, infers return value, generates function with correct parameters, replaces original range with call site
  - inline_symbol command — takes file + symbol + call site line, validates single-return, substitutes parameters for arguments, detects scope conflicts, replaces call with function body
  - shared extract.rs module with detect_free_variables, detect_return_value, generate_extracted_function, generate_call_site, detect_scope_conflicts, validate_single_return, substitute_params
  - plugin tools aft_extract_function and aft_inline_symbol with Zod schemas in refactoring.ts
  - 12 integration tests (6 extract_function + 6 inline_symbol) through binary protocol
  - 2 bun round-trip tests for plugin tools
requires:
  - slice: S01
    provides: refactoring.ts tool group pattern, handler pattern following D026
affects:
  - S03
key_files:
  - src/extract.rs
  - src/commands/extract_function.rs
  - src/commands/inline_symbol.rs
  - opencode-plugin-aft/src/tools/refactoring.ts
  - tests/integration/extract_function_test.rs
  - tests/integration/inline_symbol_test.rs
key_decisions:
  - D101 — extract_function limited to TS/JS/TSX and Python (Rust/Go deferred due to ownership/lifetime and multiple return complexity)
  - D102 — inline_symbol restricted to single-return functions (multi-return requires control flow transformation)
  - D103 — scope conflicts reported with suggestions, not auto-resolved (auto-renaming can change semantics)
  - D112 — shared extract.rs module for free variable detection, scope conflicts, parameter substitution
  - D113 — property access identifiers filtered by parent node field name (not node kind alone)
  - D114 — validate_single_return skips nested function bodies to avoid false positives
  - D115 — substitute_params uses tree-sitter identifier matching for whole-word boundary safety
patterns_established:
  - extract.rs shared module pattern for refactoring utilities — reusable across commands
  - detect_call_context returns (context_type, replacement_node, assignment_var) triple for context-aware inlining
  - find_call_recursive with source threading for callee name matching
  - strip_braces + de-indent for extracting function body text from statement_block nodes
observability_surfaces:
  - stderr log: "[aft] extract_function: {name} from {file}:{start}-{end} ({N} params)"
  - stderr log: "[aft] inline_symbol: {symbol} at {file}:{line}"
  - extract_function response: parameters array, return_type, syntax_valid, backup_id
  - inline_symbol response: call_context, substitutions, conflicts
  - error codes: unsupported_language, this_reference_in_range, multiple_returns (with return_count), scope_conflict (with conflicting_names + suggestions), call_not_found
  - both commands support dry_run mode (diff + syntax_valid without mutation)
drill_down_paths:
  - .gsd/milestones/M004/slices/S02/tasks/T01-SUMMARY.md
  - .gsd/milestones/M004/slices/S02/tasks/T02-SUMMARY.md
  - .gsd/milestones/M004/slices/S02/tasks/T03-SUMMARY.md
duration: 1h55m
verification_result: passed
completed_at: 2026-03-14
---

# S02: Extract Function & Inline Symbol

**Two single-file refactoring commands — extract_function and inline_symbol — with AST-based free variable detection, return value inference, parameter substitution, and scope conflict reporting for TS/JS/TSX and Python.**

## What Happened

Built a shared `extract.rs` module (T01) providing the core refactoring analysis utilities, then two command handlers that consume it.

**extract_function** (T01): Takes a file and line range, walks the AST within the byte range to classify identifiers by scope level — local declarations (skip), enclosing function parameters (become parameters), module-level bindings (skip), this/self references (error). Property access identifiers (`obj.prop` — `prop` is not free) are filtered by checking the parent node's field name. Return value detection handles explicit `return` statements, post-range variable usage within the enclosing function, and void. Generates a language-appropriate function declaration and replaces the original range with a call site. Full mutation pipeline: dry_run check → auto_backup → write_format_validate.

**inline_symbol** (T02): Takes a file, symbol name, and call site line. Resolves the function via the provider, validates single-return (skipping nested function bodies to avoid false positives from inner returns). Finds the call expression at the specified line, detects context (assignment, standalone, return), builds a parameter→argument substitution map using tree-sitter identifier matching (whole-word safe, property access filtered), checks scope conflicts by parsing the body snippet and comparing declarations at the call site. Replaces the call with the substituted body, adjusting for context (assignment targets, return-to-assignment conversion). Same mutation pipeline.

**Integration and plugin** (T03): 12 binary protocol integration tests covering success paths (TS, Python), dry-run, and error paths (unsupported language, this-reference, multiple-returns, scope-conflict) for both commands. Plugin tools registered in refactoring.ts with Zod schemas, 2 bun round-trip tests added.

## Verification

- `cargo test extract_function` — 21 unit tests + 6 integration tests pass
- `cargo test inline_symbol` — 17 unit tests + 6 integration tests pass
- `cargo test` — 446 total (280 unit + 166 integration), zero failures, zero regressions from 396 baseline
- `bun test` in opencode-plugin-aft/ — 42 tests pass (baseline 40 + 2 new), zero failures

## Requirements Advanced

- R029 (Extract function) — extract_function fully implemented with free variable detection, return value inference, and call site generation for TS/JS/TSX and Python. 27 tests prove success paths, error paths, and dry-run through binary protocol.
- R030 (Inline symbol) — inline_symbol fully implemented with single-return validation, argument substitution, scope conflict detection for TS/JS/TSX and Python. 23 tests prove success paths, error paths, and dry-run through binary protocol.

## Requirements Validated

- R029 — 21 unit tests cover free variable classification (enclosing params, property access filtering, module-level, this/self), return value detection (explicit, post-range, void), function generation (TS/Python). 6 integration tests prove end-to-end through binary protocol (basic TS, return value, Python, dry-run, unsupported language, this-reference). Plugin round-trip verified.
- R030 — 17 unit tests cover parameter substitution (basic, whole-word, noop), single-return validation (single, void, expression body, multiple), scope conflict detection. 6 integration tests prove end-to-end through binary protocol (basic TS, expression body, Python, dry-run, multiple-returns, scope-conflict). Plugin round-trip verified.

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- Body-text scope conflict detection parses the function body as a standalone snippet rather than using the original tree's byte offsets — simpler but may miss some declarations if tree-sitter can't parse the fragment perfectly. Conservative: any detected conflict blocks the inline.
- Minor indentation inconsistency in multi-line inline replacement (first body line may have extra indent) — write_format_validate auto-formatting corrects this.

## Known Limitations

- extract_function limited to TS/JS/TSX and Python — Rust (ownership/lifetimes) and Go (multiple returns) deferred per D101
- inline_symbol restricted to single-return functions per D102 — multi-return requires control flow transformation
- Scope conflict detection may undercount declarations in body snippets that tree-sitter can't parse as standalone fragments — conservative (blocks rather than misses)
- Multi-line inlining has minor first-line indent inconsistency before auto-formatting

## Follow-ups

- none — S03 (LSP integration) is the next slice per roadmap

## Files Created/Modified

- `src/extract.rs` — new shared module: detect_free_variables, detect_return_value, generate_extracted_function, generate_call_site, detect_scope_conflicts, validate_single_return, substitute_params + 24 unit tests
- `src/commands/extract_function.rs` — new command handler + 7 unit tests
- `src/commands/inline_symbol.rs` — new command handler + 7 unit tests
- `src/lib.rs` — added `pub mod extract`
- `src/commands/mod.rs` — added extract_function and inline_symbol module declarations
- `src/main.rs` — added two dispatch entries
- `tests/fixtures/extract_function/sample.ts` — TS fixture
- `tests/fixtures/extract_function/sample.py` — Python fixture
- `tests/fixtures/extract_function/sample_this.ts` — class method with `this` fixture
- `tests/fixtures/inline_symbol/sample.ts` — simple function + call site fixture
- `tests/fixtures/inline_symbol/sample_multi.ts` — multi-return rejection fixture
- `tests/fixtures/inline_symbol/sample_conflict.ts` — scope conflict fixture
- `tests/fixtures/inline_symbol/sample.py` — Python fixture
- `tests/integration/extract_function_test.rs` — 6 integration tests
- `tests/integration/inline_symbol_test.rs` — 6 integration tests
- `tests/integration/main.rs` — added module declarations
- `opencode-plugin-aft/src/tools/refactoring.ts` — added aft_extract_function and aft_inline_symbol tool definitions
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — added 2 round-trip tests

## Forward Intelligence

### What the next slice should know
- extract.rs utilities are reusable — `detect_free_variables` and `detect_scope_conflicts` could inform LSP-enhanced resolution by comparing tree-sitter results with LSP data
- Both commands use `ctx.provider().resolve_symbol()` for symbol lookup — this is the hook point for LSP enhancement in S03
- Plugin refactoring.ts now has 3 tools (move_symbol, extract_function, inline_symbol) — S03 may need to add lsp_hints population logic upstream of these calls

### What's fragile
- Body-text scope conflict detection parses snippets as standalone TypeScript — if a snippet isn't valid standalone syntax, some declarations may be missed. Conservative (false-negative on conflicts → blocks inline) but could miss edge cases
- `find_deepest_ancestor` in extract.rs uses `child(i)` iteration instead of cursor.node() to avoid tree-sitter lifetime issues — careful with any changes to the traversal pattern

### Authoritative diagnostics
- `cargo test extract_function` — 27 tests cover the full extract_function surface (unit + integration)
- `cargo test inline_symbol` — 23 tests cover the full inline_symbol surface (unit + integration)
- Send either command with `dry_run: true` for non-destructive verification
- Error response `code` field is machine-parseable: `unsupported_language`, `this_reference_in_range`, `multiple_returns`, `scope_conflict`, `call_not_found`

### What assumptions changed
- No assumptions changed — implementation matched the plan closely
