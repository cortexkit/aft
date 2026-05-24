# S04: Dry-run & Transactions — Research

**Date:** 2026-03-14

## Summary

S04 has two deliverables: (1) `dry_run: true` flag on all 12 mutation commands, (2) `transaction` command for multi-file atomic edits. Both are architecturally straightforward extensions of existing infrastructure.

All 12 mutation commands converge on `edit::write_format_validate()` which does fs::write → auto_format → validate_syntax → validate_full. Dry-run intercepts before fs::write: compute new content, generate a unified diff against the original using the `similar` crate, validate syntax by parsing the in-memory string (not the file on disk), and return without touching disk. The `auto_backup` call is also skipped. Per D047, the dry-run diff should show the post-format result — but since formatting requires writing to disk first (external subprocess reads the file), dry-run will show the raw edit diff only. Showing formatted dry-run output would require writing to a temp file, formatting it, reading back, then cleaning up — added complexity for marginal value. I recommend **deferring D047** (format-in-dry-run) or implementing it via temp file only if there's demand.

Transaction builds on `BackupStore::snapshot()` / `restore_latest()` — the same mechanism batch uses for single-file atomicity. The new command accepts an array of file operations, snapshots all files upfront, applies all edits, validates all, and rolls back everything on any failure. Each operation in the array is essentially a mini write/edit_match/edit_symbol dispatched against its own file.

The `similar` crate (D044) is the only new dependency. It's ~50KB, zero transitive deps, well-maintained (trust 10/10), and produces standard unified diffs via `TextDiff::from_lines().unified_diff()`. No alternatives worth considering.

## Recommendation

### Approach

Split implementation into 3 tasks:

1. **Add `similar` crate + dry-run infrastructure in `edit.rs`** — Add a new function `dry_run_diff(original: &str, proposed: &str, path: &Path) -> DryRunResult` that generates unified diff and validates syntax by parsing the proposed string in-memory. Then add `dry_run: true` param extraction to each of the 12 mutation command handlers: when dry_run is true, skip `auto_backup` and `write_format_validate`, call `dry_run_diff` instead, and return the diff + syntax validity. The 12 handlers all share the same pattern (compute `new_source`, then call `write_format_validate`), so the insertion point is consistent. Write integration tests for dry-run on representative commands (write, edit_symbol, edit_match, batch, add_import, add_member).

2. **`transaction` command** — New handler in `src/commands/transaction.rs`. Accepts `{ operations: [{ file, content?, match?, replacement?, ... }] }`. Phase model: parse all operations → snapshot all affected files → apply each operation (using existing edit infrastructure) → validate all → if any fails, restore all from backup in reverse order. Each per-file result reported individually. Write integration tests including the 3-file rollback acceptance scenario from milestone success criteria.

3. **Plugin registration** — Add `dry_run` param to all existing tool schemas in the plugin. Add `transaction` tool definition. Add bun tests.

### Why this order

Dry-run first because it's a cross-cutting concern that touches all handlers — better to do it before adding another handler (transaction). Transaction second because it's a single new command handler. Plugin last because it depends on the binary-side implementations being stable.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Unified diff generation | `similar` crate (`TextDiff::from_lines().unified_diff()`) | Standard algorithm, configurable context radius, produces parseable unified diff format. D044 mandates this. |
| Syntax validation of in-memory content | Existing `FileParser` + `tree_sitter::Parser` | Can parse a string directly with `parser.parse(content, None)` — don't need to write to disk. Currently `validate_syntax` reads from disk; need a new variant that takes `&str`. |
| File backup/restore | Existing `BackupStore::snapshot()` / `restore_latest()` | Transaction rollback uses the exact same mechanism batch already uses. No new backup infrastructure needed. |

## Existing Code and Patterns

- `src/edit.rs::write_format_validate()` — The shared mutation tail for all 12 commands. Dry-run bypasses this entirely (no disk write, no format, no validate-from-disk). Instead, a new `dry_run_diff()` function generates the diff and validates syntax from the in-memory string.
- `src/edit.rs::auto_backup()` — Skipped entirely when `dry_run: true`. No backup needed since nothing changes on disk.
- `src/edit.rs::validate_syntax()` — Reads from disk. Dry-run needs a parallel function `validate_syntax_str(content: &str, path: &Path) -> Option<bool>` that parses the string in-memory using `detect_language(path)` + `grammar_for(lang)` + `parser.parse(content, None)`.
- `src/commands/batch.rs::handle_batch()` — Closest pattern to transaction. Phases: validate → backup → apply → write/validate. Transaction follows the same phases but across multiple files.
- `src/backup.rs::BackupStore` — `snapshot()` reads file from disk and stores content. `restore_latest()` pops the entry and writes back. Transaction uses `snapshot()` for all files before any mutations, then `restore_latest()` for all files on failure.
- `src/parser.rs::detect_language()` / `grammar_for()` — Both pub, available for creating a parser to validate in-memory content during dry-run.
- `src/protocol.rs::RawRequest` — `params` is a flattened `serde_json::Value`. `dry_run` will be extracted as `req.params.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false)` — same pattern as `create_dirs`, `type_only`, etc.
- `src/commands/mod.rs` — Add `pub mod transaction;` here.
- `src/main.rs::dispatch()` — Add `"transaction" => aft::commands::transaction::handle_transaction(&req, ctx)` match arm.
- `opencode-plugin-aft/src/tools/editing.ts` — Add `dry_run` optional param to all 4 editing tools. Add transaction tool (or new file `transaction.ts`).
- `opencode-plugin-aft/src/tools/imports.ts` — Add `dry_run` optional param to all 3 import tools.
- `opencode-plugin-aft/src/tools/structure.ts` — Add `dry_run` optional param to all 5 structure tools.
- `opencode-plugin-aft/src/index.ts` — Import and spread transaction tools.

## Constraints

- **No protocol changes** — `dry_run` is an additional optional param, `transaction` is a new command string. Both fit the existing NDJSON envelope.
- **Single-threaded binary** — Transaction applies files sequentially. No parallelism concerns.
- **BackupStore is RefCell** — Transaction calls `snapshot()` and `restore_latest()` through `ctx.backup().borrow_mut()`. Must ensure the borrow is dropped between calls (same pattern used by all existing handlers — assign result, drop borrow, then proceed).
- **`similar` is the only new dep** — Per D044. No other new crates.
- **Dry-run must not touch disk** — No fs::write, no auto_backup, no formatter invocation. The diff is computed entirely in memory.
- **Plugin uses Zod re-export** (D034) — `const z = tool.schema;` not `import { z } from "zod"`.

## Dry-run Architecture Detail

### Interception point in each handler

Every mutation handler follows this pattern:
```
1. Extract params, validate
2. Read source file
3. Compute new_source (the mutation logic — different per command)
4. auto_backup(ctx, path, ...)
5. write_format_validate(path, &new_source, ctx.config(), &req.params)
6. Build response JSON from WriteResult
```

Dry-run branches after step 3:
```
3a. If dry_run:
    - Call dry_run_diff(source, new_source, path) → DryRunResult { diff, syntax_valid }
    - Return Response with { diff, syntax_valid, dry_run: true }
    - Skip steps 4-6 entirely
```

This means each handler gains ~5 lines of code: extract `dry_run` bool, then an early return after computing `new_source`. The `dry_run_diff` function is shared.

### DryRunResult struct

```rust
pub struct DryRunResult {
    pub diff: String,           // unified diff text
    pub syntax_valid: Option<bool>,  // None if unsupported language
}
```

### Response shape for dry-run

```json
{
  "ok": true,
  "id": "...",
  "dry_run": true,
  "diff": "--- a/path\n+++ b/path\n@@ ...",
  "syntax_valid": true
}
```

The response deliberately omits command-specific fields (backup_id, formatted, etc.) since no mutation occurred.

## Transaction Architecture Detail

### Command params

```json
{
  "id": "1",
  "command": "transaction",
  "operations": [
    { "file": "a.ts", "command": "write", "content": "..." },
    { "file": "b.ts", "command": "edit_match", "match": "old", "replacement": "new" },
    { "file": "c.ts", "command": "write", "content": "..." }
  ]
}
```

Each operation is a simplified write or edit. To keep scope manageable, transaction supports `write` (full content) and `edit_match` (find/replace) per file. It does NOT recursively dispatch to the full command handlers (that would be overengineered). The per-operation params are minimal: `file` + `content` for write, `file` + `match` + `replacement` for edit_match.

### Phase model

1. **Parse**: Validate all operations have required params
2. **Snapshot**: `backup.borrow_mut().snapshot(path, ...)` for each file that exists. Track which files were snapshotted.
3. **Apply**: For each operation, read current source, compute new content, call `write_format_validate()`. Collect per-file results.
4. **Validate**: Check if any operation failed (write error, syntax invalid). If syntax_valid is false for any file, consider it a failure and trigger rollback.
5. **Rollback** (on failure): For each snapshotted file in reverse order, call `backup.borrow_mut().restore_latest(path)`. Return error response with the failed operation index and per-file status.
6. **Success**: Return array of per-file results.

### Rollback trigger

The roadmap says "the third file has a syntax error — all 3 files are rolled back." This means syntax validation failure triggers rollback. The `syntax_valid == Some(false)` check after `write_format_validate` is the trigger.

However, there's a subtlety: should rollback trigger on syntax_valid=false or only on write errors? The acceptance scenario says syntax error → rollback. So: **any file with syntax_valid=Some(false) triggers full rollback** unless the user explicitly opts out. This is the safe default.

### Response shape

```json
{
  "ok": true,
  "id": "...",
  "files_modified": 3,
  "results": [
    { "file": "a.ts", "syntax_valid": true, "formatted": true },
    { "file": "b.ts", "syntax_valid": true, "formatted": false, "format_skipped_reason": "not_found" },
    { "file": "c.ts", "syntax_valid": true, "formatted": true }
  ]
}
```

On rollback:
```json
{
  "ok": false,
  "id": "...",
  "code": "transaction_failed",
  "message": "transaction rolled back: operation[2] syntax error in c.ts",
  "failed_operation": 2,
  "rolled_back": ["a.ts", "b.ts", "c.ts"]
}
```

## Common Pitfalls

- **Dry-run must skip auto_backup** — If dry-run creates a backup entry, the undo stack gets polluted with phantom entries. The `auto_backup` call must be guarded by `!dry_run`.
- **Dry-run must validate syntax from string, not file** — `validate_syntax()` reads the file from disk. During dry-run, the file hasn't changed. Need a new `validate_syntax_str()` that parses the proposed content string directly.
- **Transaction RefCell borrow scope** — `ctx.backup().borrow_mut()` must be scoped tightly. Calling snapshot in a loop while holding a borrow_mut will panic (re-entrant borrow). Solution: snapshot one file, drop borrow, loop. Same pattern used by existing `auto_backup()`.
- **Transaction rollback order** — Must restore in reverse order of application. If files A, B, C were modified and C fails, restore C first, then B, then A. This prevents partial state if a restore itself fails.
- **Transaction with new files** — If a `write` operation creates a new file (didn't exist before), rollback should delete it rather than restore from backup (there was no backup). Track which files were created vs modified.
- **similar crate `unified_diff()` header format** — `TextDiff::from_lines(old, new).unified_diff().header("a/path", "b/path")` produces standard `--- a/path\n+++ b/path` headers. Use `a/{path}` and `b/{path}` convention for git-compatible diff output.
- **Empty diff** — If dry-run produces no changes (e.g., add_import for an already-present import), the diff string will be empty. This is a valid response — the agent sees no-op.
- **Batch dry-run** — Batch applies multiple edits to one file. Dry-run for batch should show the combined diff of all edits, not individual diffs per edit. The `new_source` is already computed as the combined result.

## Open Risks

- **D047 (dry-run includes formatted result)** — The roadmap says dry-run shows the post-format diff. But formatting requires writing to disk (external subprocess reads the file). Options: (a) write to temp file, format, read back, diff, delete temp — adds complexity and temp file management; (b) show raw edit diff only — simpler, covers 95% of use cases. Recommend (b) for now, document limitation. Agents can do a real edit + undo if they need to see formatted output.
- **Transaction scope** — The roadmap says transaction supports `write` and `edit_match` per-file operations. Should it support all 12 mutation commands? That would require recursively dispatching through each handler with dry-run-like content-only computation. Recommend starting with `write` only (covers the acceptance scenario) and `edit_match` (most useful for multi-file find/replace). Other commands can be added if needed.
- **Transaction + dry-run combination** — Should `transaction` support `dry_run: true` to preview a multi-file change? Logically yes — it would show per-file diffs. This falls out naturally from the implementation: if dry_run, compute all new content and diffs without any fs::write, skip backups entirely.
- **Syntax validation as rollback trigger** — Some edits intentionally produce invalid syntax (e.g., work-in-progress code). The acceptance scenario explicitly requires rollback on syntax error. Consider adding a `rollback_on_syntax_error: true` (default) param to allow opting out.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | apollographql/skills@rust-best-practices | available (2.4K installs — general practices, not specific to this work) |
| similar (Rust diffing) | — | none found |
| tree-sitter | — | none found (relevant) |

No skills worth installing for this slice's scope.

## Sources

- `similar` crate unified diff API: `TextDiff::from_lines(old, new).unified_diff().context_radius(3).header(old_label, new_label)` — produces standard unified diff format (source: Context7 docs, trust 10/10)
- Existing codebase patterns: all 12 mutation handlers converge on `write_format_validate()`, making the dry-run interception point consistent (source: local code analysis)
- BackupStore snapshot/restore pattern from batch.rs — directly applicable to transaction rollback (source: local code analysis)
