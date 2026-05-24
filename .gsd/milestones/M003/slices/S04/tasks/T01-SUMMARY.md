---
id: T01
parent: S04
milestone: M003
provides:
  - impact command with enriched callers (signatures, entry point flags, call expressions, parameters)
  - extract_parameters utility for per-language parameter name extraction from signatures
  - aft_impact plugin tool with Zod schema
key_files:
  - src/callgraph.rs
  - src/commands/impact.rs
  - opencode-plugin-aft/src/tools/navigation.ts
  - tests/integration/callgraph_test.rs
key_decisions:
  - extract_parameters uses bracket-depth-aware comma splitting to handle generics in type annotations
  - impact() reuses callers_of's collect_callers_recursive then enriches each site (avoids duplicating traversal logic)
  - Deduplication by (file, symbol, line) tuple prevents duplicate entries from transitive expansion
patterns_established:
  - extract_parameters(signature, lang) is the shared utility for T02's argument-to-parameter matching
  - ImpactCaller struct pattern (enriched caller with metadata) available for future commands
observability_surfaces:
  - ImpactResult.total_affected and affected_files for blast radius at a glance
  - ImpactCaller.is_entry_point flags which callers are public API surfaces
  - Structured error codes: not_configured, symbol_not_found, invalid_request
duration: 30min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Impact analysis command — callers with signature and entry point annotations

**Shipped `impact` command that returns all callers of a symbol annotated with signatures, entry point status, call site source lines, and extracted parameter names.**

## What Happened

Added `ImpactResult`/`ImpactCaller` response types and `extract_parameters()` utility to `callgraph.rs`. The `extract_parameters` function handles per-language receiver skipping (Rust `&self`/`&mut self`, Python `self`), rest/spread params, type annotations, defaults, and uses bracket-depth-aware comma splitting for generics.

The `impact()` method on `CallGraph` reuses `callers_of`'s recursive traversal, then enriches each caller with its `SymbolMeta` (signature, kind, exported), computes entry point status via `is_entry_point()`, reads the source line at the call site, and extracts parameter names. Results are deduplicated and sorted deterministically.

Created `src/commands/impact.rs` following the `trace_to.rs` handler pattern exactly: param extraction, configure guard, symbol existence check, then graph method call. Wired dispatch in `main.rs` and added `aft_impact` plugin tool in `navigation.ts`.

## Verification

- `cargo test -- extract_parameters`: 15 unit tests pass (TS, JS, Python, Rust, Go, edge cases)
- `cargo test -- impact`: 3 integration tests pass (not_configured, symbol_not_found, multi_caller with entry point + signature + parameter assertions)
- `cargo test`: 363 total tests (223 unit + 140 integration), 0 failures
- `bun test`: 39 tests pass, 0 failures

Slice-level checks (T01 is intermediate — partial passes expected):
- ✅ `cargo test -- impact`: passes
- ⬜ `cargo test -- trace_data`: not yet implemented (T02)
- ✅ `cargo test`: all pass with 0 failures
- ✅ `bun test`: all pass with 0 failures

## Diagnostics

Send `{"command":"impact","file":"<path>","symbol":"<name>","depth":5}` via binary protocol. Response includes `total_affected` and `affected_files` for quick blast radius assessment. Each caller in `callers[]` has `is_entry_point`, `signature`, `call_expression`, and `parameters` fields. Error responses use standard codes: `not_configured`, `symbol_not_found`, `invalid_request`.

## Deviations

None.

## Known Issues

None.

## Files Created/Modified

- `src/callgraph.rs` — added ImpactResult, ImpactCaller types, extract_parameters() utility, split_params/extract_param_name helpers, impact() method, read_source_line helper, 15 unit tests
- `src/commands/impact.rs` — new command handler following trace_to.rs pattern
- `src/commands/mod.rs` — added `pub mod impact`
- `src/main.rs` — added `"impact"` dispatch entry
- `opencode-plugin-aft/src/tools/navigation.ts` — added aft_impact tool definition with Zod schema
- `tests/integration/callgraph_test.rs` — added 3 integration tests (not_configured, symbol_not_found, multi_caller)
- `.gsd/milestones/M003/slices/S04/tasks/T01-PLAN.md` — added Observability Impact section
