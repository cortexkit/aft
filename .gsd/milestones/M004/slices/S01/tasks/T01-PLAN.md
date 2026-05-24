---
estimated_steps: 8
estimated_files: 5
---

# T01: Implement move_symbol command handler with relative path computation

**Slice:** S01 — Move Symbol with Import Rewiring
**Milestone:** M004

## Description

Build the `move_symbol` command handler — the core Rust implementation that orchestrates moving a top-level symbol from one file to another with full import rewiring across all consumer files. This is the highest-complexity task in the slice: it coordinates symbol resolution, call graph consumer discovery, multi-file mutations with checkpoint safety, relative path computation, and dry_run support.

The command's flow: resolve symbol → verify top-level → checkpoint → discover consumers via `callers_of` → extract symbol text → remove from source → add to destination → rewrite imports in every consumer → format/validate all files → return results.

## Steps

1. **Create `src/commands/move_symbol.rs`** with `handle_move_symbol(req, ctx)` following D026. Extract params: `file` (source), `symbol` (name), `destination` (target file path), optional `scope` (disambiguation), optional `dry_run`. Validate all required params upfront.

2. **Implement the call graph and symbol guards.** Check `ctx.callgraph()` is `Some` (return `not_configured` per D089). Resolve symbol via `ctx.provider().resolve_symbol()`. Verify it's top-level: reject if `scope_chain` is non-empty or `kind` is `Method` (D100). Handle disambiguation if multiple matches.

3. **Build the relative path computation utility.** Given a consumer file path and a destination file path, compute the correct relative import path (e.g., `./utils`, `../shared/utils`, `../../lib/helpers`). Must handle: same directory (`./`), parent directories (`../`), deeply nested paths. Strip file extensions for TS/JS/TSX (import paths don't include `.ts`). Unit-testable as a pure function.

4. **Implement the multi-file mutation pipeline.** (a) Create auto-checkpoint with name `move_symbol:{symbol_name}` (D105). (b) Read source file, extract symbol text (full declaration including preceding decorators/comments but excluding export keyword — handle export separately). (c) Remove symbol from source file + remove/update export statement. (d) Write/append symbol to destination file with `export` prefix. (e) For each consumer from `callers_of`: parse imports, find the import referencing the source file with the moved symbol name, recompute the import path to point to destination, regenerate the import statement preserving aliases. (f) All writes go through `write_format_validate()`.

5. **Implement dry_run mode.** When `dry_run: true`, compute all diffs (source removal, destination addition, consumer rewrites) without writing to disk. Return `{ ok, dry_run, diffs: [{ file, diff }] }`.

6. **Implement rollback on failure.** If any `write_format_validate` fails, restore checkpoint. Return error with `failed_file` and `rolled_back` list.

7. **Wire into dispatch and module registry.** Add `pub mod move_symbol;` to `src/commands/mod.rs`. Add `"move_symbol" => aft::commands::move_symbol::handle_move_symbol(&req, ctx)` to `dispatch()` in `src/main.rs`.

8. **Create multi-file fixture set** in `tests/fixtures/move_symbol/`. Need: source file with 2+ exported functions (one to move), destination file (may already exist with some content), 5+ consumer files at different relative paths importing the symbol to move. Include one consumer using aliased import. Include one consumer in a subdirectory (tests relative path with `../`). Include a consumer that imports multiple symbols from the source (move should only update the moved symbol's import, not the others).

## Must-Haves

- [ ] `handle_move_symbol` follows `handle_*(req, ctx)` signature (D026)
- [ ] Returns `not_configured` when call graph not initialized (D089)
- [ ] Rejects non-top-level symbols (methods, class members) with clear error (D100)
- [ ] Auto-checkpoint before mutations with symbol name in checkpoint name (D105)
- [ ] All file writes through `write_format_validate()` (D046, D066)
- [ ] Dry-run returns multi-file diffs without touching disk (D071)
- [ ] Relative path computation handles same-dir, parent-dir, and deep nesting correctly
- [ ] Consumer import rewriting preserves aliases
- [ ] Fixture set has 5+ consumer files with varied import patterns

## Verification

- `cargo build` compiles without errors
- `cargo test --lib` passes (no regressions from existing unit tests)
- Fixture files are syntactically valid (manually verifiable from file content)
- The `move_symbol` command is reachable in dispatch (grepping main.rs confirms the match arm)

## Inputs

- `src/callgraph.rs` — `CallGraph::callers_of()` for consumer discovery, `CallGraph::build_file()` for file data
- `src/imports.rs` — `parse_imports()`, `find_insertion_point()`, `generate_import_line()`, `is_duplicate()` for import manipulation
- `src/edit.rs` — `write_format_validate()`, `replace_byte_range()`, `line_col_to_byte()`, `dry_run_diff()`
- `src/language.rs` — `LanguageProvider::resolve_symbol()`, `list_symbols()` for symbol resolution
- `src/checkpoint.rs` — `CheckpointStore::create()` for auto-checkpoint
- `src/context.rs` — `AppContext` struct with all stores
- `src/commands/edit_symbol.rs` — reference for symbol resolution + disambiguation pattern
- `src/commands/transaction.rs` — reference for multi-file mutation + rollback pattern
- `tests/fixtures/callgraph/` — reference for fixture structure

## Expected Output

- `src/commands/move_symbol.rs` — complete command handler (~300-400 lines)
- `src/commands/mod.rs` — updated with `pub mod move_symbol;`
- `src/main.rs` — updated with `move_symbol` dispatch entry
- `tests/fixtures/move_symbol/` — 6-8 fixture files forming a multi-file project with import relationships

## Observability Impact

- **Response signals:** `files_modified` count, `consumers_updated` count, `checkpoint_name` for rollback identification, per-file `syntax_valid` and `formatted` status in results array
- **Inspection surfaces:** `list_checkpoints` command shows the auto-created `move_symbol:{name}` checkpoint; `callers` command verifies consumer list pre/post move; dry_run returns full multi-file diff preview
- **Failure visibility:** On partial failure, checkpoint enables full rollback; error response includes `failed_file` path and `rolled_back` array listing all restored/deleted files with action taken
- **Stderr logging:** `[aft] move_symbol: {symbol} from {source} to {destination} ({N} consumers updated)` on success; `[aft] move_symbol failed: ...` with rollback details on failure
- **Future agent inspection:** A future agent can verify the command is wired by grepping `"move_symbol"` in `src/main.rs` dispatch; can inspect checkpoint store via `list_checkpoints`; can verify consumer rewiring by running `callers` on the destination file post-move
