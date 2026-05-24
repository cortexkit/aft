---
estimated_steps: 8
estimated_files: 7
---

# T01: Build extract_function command with free variable detection

**Slice:** S02 — Extract Function & Inline Symbol
**Milestone:** M004

## Description

Build the core `extract_function` command handler and the shared `extract.rs` module containing free variable detection and return value inference. This is the heaviest task in the slice — the free variable classification algorithm is the novel piece that distinguishes parameters from module-level bindings from `this`/`self` references. The utilities built here are directly reused by T02's inline_symbol for scope conflict detection.

## Steps

1. Create `src/extract.rs` with `detect_free_variables(source, tree, start_byte, end_byte, lang)` — walk all `identifier` nodes in the byte range, collect references, walk all declaration nodes in the range to find locals, walk upward to find enclosing function scope boundary, classify each reference as: declared-in-range (local, skip), declared-in-enclosing-function (parameter), declared-at-module-scope (skip), `this`/`self` keyword (flag). Use node kinds from callgraph.rs vocabulary: `variable_declarator`, `assignment_expression`, `let_declaration` for declarations; `identifier` vs `property_identifier`/`field_identifier` distinction for references (D: property access identifiers are NOT free variables).
2. Add `detect_return_value(source, tree, start_byte, end_byte, enclosing_fn_end_byte, lang)` to `extract.rs` — scan the range for `return_statement` nodes (explicit return → return expression), scan declarations in-range against identifier references in post-range code within the enclosing function (variable used after → return value), otherwise void.
3. Add `generate_extracted_function(name, params, return_expr, body_text, indent, lang)` to `extract.rs` — produce function declaration text for TS/JS or Python with correct indentation using `detect_indent`.
4. Create `src/commands/extract_function.rs` with `handle_extract_function(req, ctx)` following the edit_symbol.rs pattern: validate params (file, name, start_line, end_line) → read file → detect_language guard (D101) → parse AST → convert line range to byte range via `line_col_to_byte` → detect free variables → check for `this`/`self` error → detect return value → generate function text → generate call site text → compute new file content (insert function before enclosing function, replace range with call) → dry_run check → auto_backup → write_format_validate → build response JSON with parameters, return_type, ranges.
5. Add `pub mod extract;` to `src/lib.rs`, `pub mod extract_function;` to `src/commands/mod.rs`, dispatch entry `"extract_function"` to `src/main.rs`.
6. Create test fixtures in `tests/fixtures/extract_function/` — at minimum: `sample.ts` (function with a block containing local vars, module-level refs, and function-scope refs), `sample.py` (equivalent Python), `sample_this.ts` (method with `this` reference in target range).
7. Write unit tests in `extract.rs` for: free variable detection with simple identifiers, property access filtering, module-level vs function-level classification, `this`/`self` detection, return value via explicit return, return value via post-range usage, void function detection. Write unit tests in `extract_function.rs` for: param validation, unsupported language rejection.
8. Run `cargo test extract_function` and `cargo test` to verify zero regressions.

## Must-Haves

- [ ] `detect_free_variables` correctly classifies: local declarations (not params), enclosing function scope refs (params), module-level refs (not params), `this`/`self` (error flag)
- [ ] Property identifiers in dot access (`obj.prop` — `prop` is NOT a free variable) are correctly filtered
- [ ] `detect_return_value` handles: explicit return, post-range variable usage, void
- [ ] `handle_extract_function` validates params, guards on D101 languages, computes correct function + call site text
- [ ] dry_run mode returns diff without modifying file
- [ ] auto_backup called before mutation
- [ ] write_format_validate called for the mutation
- [ ] Unsupported language returns `unsupported_language` error code
- [ ] `this`/`self` in range returns `this_reference_in_range` error code with recommendation

## Verification

- `cargo test extract_function` — all unit tests pass (free variable detection, return value, handler validation)
- `cargo test` — full suite passes, zero regressions against 396 baseline
- Manual spot-check: send an `extract_function` command via the binary for a TS fixture and verify the output JSON has correct parameters and the file is correctly modified

## Inputs

- `src/edit.rs` — `line_col_to_byte`, `auto_backup`, `write_format_validate`, `dry_run_diff`, `is_dry_run` (proven mutation pipeline)
- `src/parser.rs` — `detect_language`, `grammar_for`, `node_text`, `node_range` (AST utilities)
- `src/indent.rs` — `detect_indent` (indentation matching)
- `src/callgraph.rs` — node kind vocabulary for declarations (pattern reference, not direct import)
- `src/commands/edit_symbol.rs` — handler pattern reference

## Expected Output

- `src/extract.rs` — new shared module with `detect_free_variables`, `detect_return_value`, `generate_extracted_function` + unit tests (~300-400 lines)
- `src/commands/extract_function.rs` — command handler + param validation unit tests (~250-350 lines)
- `tests/fixtures/extract_function/` — 3+ fixture files for TS, Python, this-reference scenarios
- Dispatch wired in main.rs, modules registered in lib.rs and commands/mod.rs

## Observability Impact

- **stderr log**: `[aft] extract_function: {name} from {file}:{start}-{end} ({N} params)` on successful mutation — enables tracing what extractions happened in a session
- **Response JSON**: includes `parameters` (list of detected free variable names), `return_type` ("expression" | "variable" | "void"), `extracted_range` and `call_site_range` — agent can verify the extraction is correct without re-reading the file
- **Error codes**: `unsupported_language` (file type not TS/JS/Python), `this_reference_in_range` (range contains `this`/`self` — suggest extracting as method instead) — both machine-parseable for agent retry logic
- **Dry-run mode**: `dry_run: true` returns unified diff + syntax validity without mutation — agent can preview before committing
- **Future agent inspection**: a future agent can verify extract_function worked by checking the response `parameters` array against the source code, or by re-reading the file and confirming the function exists at `extracted_range`
