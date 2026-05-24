# S03: Auto-format & Validation — Research

**Date:** 2026-03-14

## Summary

S03 adds two capabilities: (1) auto-format after every mutation, and (2) opt-in full type-checker validation. Both require the same infrastructure — spawning external tools as subprocesses with timeout protection and graceful not-found handling. The binary is single-threaded (D014), so external tool invocation blocks the main thread, which is acceptable for both use cases: auto-format is fast (<1s for file-level formatting), and full validation is explicitly opt-in (D007).

The current codebase has 12 mutation commands, all following the same lifecycle: params → validate → compute content → `auto_backup` → `fs::write` → `validate_syntax` → respond. The auto-format hook inserts between `fs::write` and `validate_syntax` (D046). Rather than touching all 12 commands individually, the right approach is a shared `write_format_validate` function in `src/edit.rs` that replaces the raw `fs::write` + `validate_syntax` calls. This centralizes the format step and ensures all future mutation commands inherit formatting automatically.

The `validate: "full"` mode (R017) is simpler — it's an optional parameter on mutation commands that triggers a type-checker subprocess after the edit. The response includes any type errors as structured data. This is entirely additive and doesn't affect the core pipeline.

## Recommendation

Build from the bottom up: external tool runner → formatter detection → format hook in edit pipeline → update all mutation commands → full validation → plugin updates.

**Task breakdown:**

1. **T01: External tool runner + auto-format infrastructure** — New `src/format.rs` module with subprocess runner (timeout, kill, NotFound handling), formatter detection per language, `auto_format` function. Extend Config with timeout fields. Unit tests for timeout, not-found, and happy-path formatting.

2. **T02: Edit pipeline integration + mutation command updates** — New `write_format_validate()` function in `src/edit.rs` that replaces the repeated `fs::write` → `validate_syntax` pattern across all 12 mutation commands. Each command gets `formatted` and `format_skipped_reason` response fields. Integration tests for format-applied and format-not-found paths.

3. **T03: Full validation mode + plugin updates** — Type checker detection and invocation per language. `validate: "full"` parameter support on all mutation commands. Plugin tool definitions updated with validate param and format response documentation. Integration tests for validation paths.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Subprocess spawning | `std::process::Command` | Rust stdlib. Resolves PATH automatically, returns `io::ErrorKind::NotFound` for missing binaries. No external crate needed. |
| Subprocess timeout | `child.try_wait()` polling loop with `Instant` timer | Simple, single-threaded compatible. 50ms poll interval is negligible for tools running 1-10s. No threads or async runtime needed. |
| Formatter not-found detection | `Command::new(name).spawn()` error kind check | `ErrorKind::NotFound` is the exact signal. No need for a `which` crate — just attempt spawn and handle the error. |
| Formatter invocation | `prettier --write`, `rustfmt`, `ruff format`, `gofmt -w` | All standard formatters accept file paths and modify in-place. Standard CLI interfaces, well-documented. |
| Type checker invocation | `tsc --noEmit`, `pyright`, `cargo check`, `go vet` | Standard CLI tools with parseable output. Errors include file:line:message format. |

## Existing Code and Patterns

- `src/edit.rs` — Core edit engine. Functions: `line_col_to_byte`, `replace_byte_range`, `validate_syntax`, `auto_backup`. This is where the format hook goes (D046). `validate_syntax` uses a fresh `FileParser` (D023) — the format step must run before this so we validate the formatted content, not the pre-format content.

- `src/config.rs` — `Config` struct with `project_root`, `validation_depth`, `checkpoint_ttl_hours`, `max_symbol_depth`. Needs two new fields: `formatter_timeout_secs` (default 10) and `type_checker_timeout_secs` (default 30) per D043.

- `src/context.rs` — `AppContext` threads `Config` to handlers. Handlers access via `ctx.config()`. Format function needs `ctx.config().project_root` for config-file discovery and `ctx.config().formatter_timeout_secs` for timeout.

- `src/parser.rs` — `detect_language()` maps file extension to `LangId`. Format function uses this to select the correct formatter. `LangId` enum: TypeScript, Tsx, JavaScript, Python, Rust, Go.

- All 12 mutation command handlers (`write`, `edit_symbol`, `edit_match`, `batch`, `add_import`, `remove_import`, `organize_imports`, `add_member`, `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`) — Each has the same `fs::write` → `validate_syntax` section that gets replaced by the shared `write_format_validate` call.

- `src/error.rs` — `AftError` enum with `code()` method. May need a new variant for external tool failures, or format errors can be soft (reported in response, not as errors).

- `tests/integration/helpers.rs` — `AftProcess` test helper. All integration tests use this. No changes needed — new tests follow the same pattern.

- `opencode-plugin-aft/src/tools/editing.ts` — Plugin tool definitions. All mutation tools need an optional `validate` param. Response descriptions need updating to document `formatted`/`format_skipped_reason` fields.

## Constraints

- **Single-threaded binary** (D014/D029) — External tool invocation blocks the main thread. No async, no threads for subprocess management. Polling `try_wait()` with sleep is the approach.
- **No protocol changes** — New params (`validate`) are optional additions to existing flattened params. New response fields (`formatted`, `format_skipped_reason`, `validation_errors`) are additive. No envelope changes.
- **No new Cargo dependencies** — Subprocess management uses `std::process`. No external crates needed for S03. (The `similar` crate from D044 is for S04/dry-run, not S03.)
- **Format before validate** — The auto-format must run before `validate_syntax` so we validate the formatted content. The response's `syntax_valid` reflects the final state.
- **Formatter modifies file in-place** — All standard formatters (`prettier --write`, `rustfmt`, `ruff format`, `gofmt -w`) modify the file directly. No stdin/stdout piping of content needed for formatters.
- **Type checkers read but don't modify** — Type checkers produce diagnostic output on stdout/stderr. Parse their output for error messages.
- **Config walk to project root** — Walk up from the file's directory to `project_root` (or `.git` boundary) looking for formatter config files. Take the nearest config found.
- **Web-first priority** (D004) — TS/JS/TSX (prettier) first, then Python (ruff/black), then Rust (rustfmt), Go (gofmt).

## Common Pitfalls

- **Format changes file content after backup** — The backup captures pre-mutation state. Formatting happens after mutation, further changing the file. This is correct: undo restores the original pre-mutation content, not the formatted-but-unformatted intermediate. The backup point is before the entire mutation+format cycle.

- **Validate syntax after format, not before** — If validate_syntax runs before formatting, a formatter might fix syntax issues that would have been reported as errors. Always: write → format → validate.

- **Formatter exit code ≠ 0 doesn't mean failure** — Some formatters use non-zero exit for "file was already formatted" or "file had issues that were fixed." Check stderr for actual errors. For prettier: exit 0 = success. For rustfmt: exit 0 = success, exit 1 = error. Treat non-zero as "format attempted but may have failed" — still check if the file content changed.

- **Subprocess not killed on panic** — If the Rust binary panics while waiting for a subprocess, the child process leaks. Since we use `panic = "abort"` in release, the child gets orphaned. Not a real issue in practice (binary panics are bugs), but worth noting.

- **Python formatter preference: ruff vs black** — Both are valid. Strategy: try `ruff format` first (faster, modern). If ruff is not found, try `black`. If neither is found, skip. Don't make the agent choose.

- **`cargo check` is project-level, not file-level** — Unlike other type checkers, `cargo check` validates the entire project, not a single file. This makes it slower and its output includes errors from other files. For S03, still invoke it per D043's 30s timeout — it's opt-in. Filter output to show only errors related to the edited file if feasible.

- **`tsc` availability** — `tsc` requires TypeScript installed in the project or globally. Many TS projects use it via `npx tsc`. Strategy: try `npx tsc` first, fall back to `tsc` on PATH. If neither works, report not-found.

- **gofmt is always the formatter for Go** — Go has a single canonical formatter with no configuration. If `gofmt` is on PATH, use it. No config detection needed.

- **rustfmt uses the toolchain's version** — `rustfmt` may differ across Rust toolchain versions. Always use the `rustfmt` on PATH (which corresponds to the active toolchain). No version detection needed.

## Open Risks

- **Formatter changes break syntax** — In theory, formatters should never introduce syntax errors. In practice, formatter bugs exist. Mitigation: `validate_syntax` after format catches this, and the file has a backup. Low risk.

- **Type checker timeout too short for large projects** — `cargo check` on a large Rust project can exceed 30s on first run (cold cache). The configurable timeout (D043) mitigates this, but the default may surprise users. Document the behavior.

- **Multiple formatters configured** — A project might have both `.prettierrc` and a biome config. Strategy: check for formatters in a fixed priority order per language and use the first found. Don't try to detect conflicts.

- **Formatter not idempotent when run from binary CWD** — Some formatters resolve config relative to CWD, not the file path. Must ensure we run the formatter with the correct working directory (file's parent dir or project root). Use `Command::current_dir()`.

- **Format response field semantics** — Need clear semantics for the `formatted` field: `true` means "formatter ran and exited successfully", `false` means "formatter was not run" (with `format_skipped_reason` explaining why: "not_found", "timeout", "error", "unsupported_language"). This is not a boolean "did the content change."

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust | apollographql/skills@rust-best-practices | available (2.4K installs — general Rust practices, not specific to subprocess/formatter integration) |
| Rust subprocess | mohitmishra786/low-level-dev-skills@rust-debugging | available (30 installs — debugging focus, not relevant) |

No skills are directly relevant. The work is Rust stdlib subprocess management (`std::process`) with domain-specific formatter/type-checker integration. No external skills recommended.

## Sources

- Formatter CLI interfaces verified locally: `prettier --write` (v3.8.1), `rustfmt` (stable), `ruff format` (available), `black` (available), `gofmt -w` (available) — all confirmed to modify files in-place
- `std::process::Command` error handling: `spawn()` returns `Err` with `io::ErrorKind::NotFound` when binary not on PATH (source: Rust std docs)
- `child.try_wait()` for non-blocking subprocess status check: returns `Ok(None)` if not yet exited, `Ok(Some(status))` on exit (source: Rust std docs)
- Formatter config file names: prettier (12 config file variants), rustfmt (2), ruff (3), black (2), gofmt (none) — verified against each tool's documentation
- Type checker CLI: `tsc --noEmit` for TS, `pyright` for Python, `cargo check` for Rust, `go vet` for Go — standard invocations (source: respective tool documentation)
