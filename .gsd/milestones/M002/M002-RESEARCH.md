# M002: Language Intelligence — Research

**Date:** 2026-03-14

## Summary

The M001 codebase is clean, well-structured, and ready for extension. The ~5300-line Rust binary follows a consistent pattern: each command lives in `src/commands/*.rs` with a `handle_*` function taking `(&RawRequest, &AppContext) -> Response`. The AppContext dispatch model (D025/D026), per-file backup system, and tree-sitter parser infrastructure provide solid foundations for M002's features. The binary is single-threaded with RefCell interior mutability — this is fine for M002.

M002's riskiest work is **import management** (R013). Tree-sitter exposes import nodes with clear, language-specific node types (`import_statement` for TS/JS, `import_from_statement` for Python, `use_declaration` for Rust, `import_declaration` for Go), but the logic to parse, group, deduplicate, merge, and rewrite imports is pure application code — no library handles this. This is unavoidably hand-rolled, but the tree-sitter AST gives us all the structural information we need. The second risk is **external tool invocation** (formatters/type checkers) — spawning subprocesses with timeout protection and graceful missing-tool handling. The third risk is **multi-file transactions** — but these build directly on the existing BackupStore and are architecturally simple.

Primary recommendation: **start with dry-run mode and transactions** (S05 in the context) as the first slice, because they're architecturally simple extensions of existing infrastructure and immediately useful. Then do import management (highest risk, highest value), then scope-aware insertion, then auto-format + validation (external tool plumbing), then compound operations last (most language-specific, least risky given the patterns established by earlier slices).

## Recommendation

Prove the hardest things first, but only after establishing the infrastructure those hard things depend on:

1. **Dry-run + Transactions** — Extend existing mutation commands with `dry_run: true` support and add a `transaction` command. Both build on BackupStore and the existing edit engine. Low risk, high utility, establishes the pattern for all later slices. Transaction atomicity is the backbone for import reorganization and compound operations.

2. **Import Management** — Highest risk, highest value. Needs new tree-sitter query patterns to locate import blocks, plus per-language logic for grouping, deduplication, alphabetization, and merging. Start with TypeScript (largest user base, clearest import structure), then extend to the other 5 languages.

3. **Scope-aware member insertion** — Needs tree-sitter scope resolution (partially exists via `scope_chain` in Symbol) plus indentation detection. Python is the hard case (indent-as-scope).

4. **Auto-format + Validation** — External subprocess invocation with config detection. Formatter and type checker integration share the same plumbing (spawn, timeout, parse output). Build once, use for both.

5. **Compound operations** — Language-specific transforms. By this point, all the infrastructure (import management, AST manipulation, auto-format) exists. These are mostly new command handlers that compose existing capabilities.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Tree-sitter parsing, AST access | `tree-sitter` crate (already embedded) | Already provides all import node types needed — `import_statement`, `use_declaration`, `import_declaration`, `import_from_statement`. No additional parsing libraries needed. |
| Subprocess timeout | `std::process::Command` + thread-based timeout | Rust stdlib is sufficient. Use `std::thread::spawn` with `child.wait()` + `child.kill()` on timeout. No need for `tokio` or async runtime — binary is single-threaded synchronous. |
| Unified diff generation | `similar` crate (Rust) | For dry-run diff output. Small, well-maintained, produces standard unified diff. Don't hand-roll diff algorithms. |
| Import sort order (Python) | isort's grouping convention | Follow isort's 3-group convention (stdlib → third-party → local) as the default. Don't invent a new grouping scheme. |
| Import sort order (Go) | goimports' grouping convention | Follow goimports' 3-group convention (stdlib → third-party → internal). |
| Import sort order (TS/JS) | No universal standard; use the simple 3-group: external → internal → relative | Match the most common convention. Configurable later. |
| Import sort order (Rust) | rustfmt's convention | `std/core/alloc` → external crates → `crate::` → `super::` → `self::` |

## Existing Code and Patterns

- `src/main.rs` — Dispatch table in `dispatch()` function. Each new command is a single `match` arm mapping to `handle_*` function. Extend for all new commands (`add_import`, `remove_import`, `organize_imports`, `add_member`, `add_derive`, etc.).
- `src/commands/mod.rs` — Module registry. Add new modules here.
- `src/protocol.rs` — `RawRequest` with flattened params and `Response` with success/error builders. All new commands use the same envelope. The `dry_run` flag will be an optional param on all mutation commands. No protocol changes needed (constraint from context).
- `src/context.rs` — `AppContext` threads `LanguageProvider`, `BackupStore`, `CheckpointStore`, `Config` to handlers. May need extension if we add formatter config cache or external tool registry to context.
- `src/edit.rs` — `auto_backup()`, `validate_syntax()`, `replace_byte_range()`, `line_col_to_byte()`. Core edit engine reused by all mutation commands. Dry-run will call `replace_byte_range` but skip `fs::write` and `auto_backup`.
- `src/backup.rs` — `BackupStore` with `snapshot()` and `restore_latest()`. Transaction rollback will use `snapshot()` per-file before mutation, then `restore_latest()` on failure — the exact same mechanism batch already uses for single-file atomicity.
- `src/parser.rs` — `FileParser` with cache, `extract_symbols()`, and all 6 language extractors. Import management needs new extraction functions (not symbol queries — import nodes are separate from function/class/struct queries). The `detect_language()` and `grammar_for()` functions are reusable for import parsing.
- `src/parser.rs::node_range()`, `node_text()` — Utility functions for converting tree-sitter nodes to Range and extracting text. Reuse extensively.
- `src/commands/batch.rs` — `handle_batch()` is the closest pattern to `transaction`. Batch is single-file atomic; transaction is multi-file atomic. Same phases: validate → backup → apply → validate → (rollback on failure).
- `src/commands/edit_symbol.rs` — Shows the full mutation lifecycle: resolve → backup → edit → validate → respond. All new mutation commands follow this pattern.
- `opencode-plugin-aft/src/tools/editing.ts` — Tool registration pattern with Zod schemas. Each new command needs a corresponding tool definition with args schema and execute function.
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge class. No changes needed — new commands flow through the same `bridge.send()` mechanism.
- `tests/integration/helpers.rs` — `AftProcess` test helper. All integration tests use this pattern.

## Constraints

- **No protocol changes** — All new commands use the existing NDJSON request/response envelope. New commands are new `command` strings; new params are additional fields in the flattened params object.
- **Single-threaded binary** — RefCell interior mutability pattern (D014, D029). No Mutex, no async. External tool invocation blocks the main thread — acceptable for opt-in operations (R017: "synchronous because it's opt-in").
- **Web-first language priority** (D004) — TS/JS/TSX first in every slice, then Python, then Rust, Go.
- **No new Cargo dependencies without justification** — Currently: serde, serde_json, tree-sitter + 5 grammars, streaming-iterator. The `similar` crate for diff generation (dry-run mode) would be the only justified addition.
- **Formatter/type checker timeouts** — External tools must not hang the binary. Default timeout: 10 seconds. Must be killable.
- **BackupStore is in-memory** — Transaction rollback relies on the same in-memory store. A crash mid-transaction loses rollback data, but the per-file backups written to disk by `auto_backup()` before each mutation provide a recovery path.
- **Plugin uses Zod re-export** (D034) — New tool schemas must use `tool.schema` not direct `zod` import.

## Common Pitfalls

- **Import block detection is position-dependent** — Imports aren't always at the top of the file. Rust `use` statements can appear inside function bodies. Python imports can appear after `if TYPE_CHECKING:`. Must scan for the *correct* import block, not just "all import nodes." Solution: for M002, target top-level imports only (direct children of the program/module root node). Document this limitation.
- **Python indentation calculation** — Inserting a method into a Python class requires matching the indentation of existing methods. Off-by-one indentation breaks semantics (body vs. next statement). Solution: detect indentation from the target scope's existing children. If the class body uses 4-space indent, use 4-space indent. Never assume a fixed indent width.
- **Rust `use` tree merging is complex** — `use std::path::{Path, PathBuf}` is a tree structure. Adding a new import to an existing `use` tree means either appending to the `use_list` or creating a new `use_declaration`. Tree merging is the most complex single operation. Solution: start with "add new `use` declaration" (no merging). Add `organize_imports` separately to merge/sort after the fact.
- **Go import block has implicit grouping by blank lines** — Go's `import (...)` uses blank lines to separate groups (stdlib, third-party, internal). Preserving or reconstructing these groups requires understanding the blank-line convention. Solution: when adding an import, detect which group it belongs to and insert within that group. When organizing, reconstruct groups from scratch.
- **TypeScript `import type` vs `import`** — Type-only imports (`import type { Foo }`) are semantically different and must be preserved. Solution: the tree-sitter AST has a `type` child node on type imports — use this to distinguish.
- **External tool not found** — Formatter/type checker not installed on user's system. Must not error — must report "formatter_not_found" in the response and skip formatting. Solution: `which`/`command -v` check before spawning. Return `{ formatted: false, reason: "prettier not found" }`.
- **Transaction rollback ordering** — If file 3 of 5 fails, files 1 and 2 have already been written to disk. Must restore them from backup in reverse order. Solution: backup all files first, apply all, validate all, rollback all on any failure.
- **Dry-run must not touch disk** — Dry-run generates a diff from the proposed edit without writing. The current `auto_backup` → `fs::write` → `validate_syntax` pipeline must be short-circuited. Solution: compute the new content string, generate diff against original, validate by parsing the string (not the file), return without writing.

## Open Risks

- **Import grouping conventions vary across projects** — Some projects use custom import orders (e.g., internal packages first). M002 ships with language-standard defaults. Future: detect project conventions from existing imports or config files. This is a "good enough" vs "perfect" tradeoff — recommend shipping defaults and deferring convention detection.
- **Formatter config discovery depth** — How far up the directory tree to search for `.prettierrc`, `rustfmt.toml`, etc.? The context says "walk up to project root (where .git is)." Risk: monorepo with per-package configs. Solution: walk up to `.git`, take the *nearest* config file.
- **Type checker invocation time** — `cargo check` on a large project can take 30+ seconds. The 10-second default timeout may be too short. Consider making it configurable via Config. Risk is user frustration, not correctness — the operation is opt-in.
- **Tree-sitter version compatibility** — Using tree-sitter 0.24 with grammar 0.23 crates. These are pinned in Cargo.toml and stable. No upgrade pressure for M002, but worth noting.
- **Compound operations are highly language-specific** — `add_derive` (Rust only), `wrap_try_catch` (TS/JS only), `add_decorator` (Python only), `add_struct_tags` (Go only). Each requires deep understanding of that language's AST structure. Risk: scope creep per operation. Mitigation: treat each as a small, self-contained handler.

## Candidate Requirements

The following are not in the current requirements but emerged from research. They should be discussed during planning, not silently added.

- **CR-001: Top-level import scope only** — M002 import management should target only top-level imports (direct children of program root), not imports inside conditional blocks (`if TYPE_CHECKING:` in Python, `#[cfg]` in Rust). This simplifies the implementation significantly and covers >95% of use cases. Deeper import scoping could be deferred.
- **CR-002: Indentation detection** — No existing infrastructure for detecting a file's indentation style (tabs vs spaces, indent width). Scope-aware insertion (R014) and compound operations (R015) both need this. Should be a shared utility, not per-command logic.
- **CR-003: External tool timeout configuration** — The current `Config` struct has no timeout field. Formatter and type checker invocations need configurable timeouts. Default: 10s for formatters, 30s for type checkers.
- **CR-004: `similar` crate dependency** — Dry-run mode needs diff generation. The `similar` crate is small (~50KB), well-maintained, and produces standard unified diffs. Adding it as a dependency is cleaner than hand-rolling diff logic.

## AST Node Types for Import Management

Critical reference for implementation — tree-sitter node kinds per language:

| Language | Import Node Kind | Key Children |
|----------|-----------------|--------------|
| TypeScript/TSX | `import_statement` | `import_clause` (→ `named_imports`, `namespace_import`, or `identifier`), `string` (source), optional `type` keyword |
| JavaScript | `import_statement` | Same as TS minus `type` keyword |
| Python | `import_statement`, `import_from_statement` | `dotted_name` (module), named imports as `dotted_name` children |
| Rust | `use_declaration` | `scoped_identifier` or `scoped_use_list` (tree imports), optional `visibility_modifier` |
| Go | `import_declaration` | `import_spec_list` → `import_spec` → `interpreted_string_literal` |

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | plurigrid/asi@tree-sitter | available (7 installs — low adoption, unlikely to be useful) |
| Rust | apollographql/skills@rust-best-practices | available (2.4K installs — general Rust practices, not specific to this work) |

No skills are directly relevant enough to recommend installing. The work is domain-specific (AST manipulation, import management) and doesn't map to general-purpose skills.

## Sources

- Tree-sitter AST node types verified by building and running a test binary against tree-sitter 0.24 with all 6 grammar crates — confirmed `import_statement`, `use_declaration`, `import_declaration`, `import_from_statement` node kinds and their child structures (source: local experimentation)
- isort import grouping convention: stdlib → third-party → first-party is the standard 3-section ordering (source: isort documentation convention, widely adopted)
- goimports convention: stdlib → third-party → internal with blank-line separators (source: Go tooling convention)
- Rust import ordering convention: `std`/`core`/`alloc` → external crates → `crate::` → `super::` → `self::` (source: rustfmt defaults)
- `similar` crate for Rust diff generation: unified diff output, ~50KB, no transitive dependencies (source: crates.io)
