# Codebase Structure

A directory map. How the pieces work together is in
[ARCHITECTURE.md](ARCHITECTURE.md). For individual files, search the code:
it stays current and this document does not list them.

## Top level

```text
crates/aft/            `agent-file-tools`: the `aft` binary, CLI, daemon edge, tool pipeline, commands, integration tests
crates/aft-engine/     Shared runtime: project contexts, indexes, edit engines, LSP, watchers, shell runtime
crates/aft-base/       Shared foundation: protocol, config, executor, bash compression, secrets, locks, URL fetch
crates/aft-db/         SQLite stores (`aft.db`), access policy, sandbox profile, write ledger
crates/aft-parse/      Syntax parsing, grammars, imports, fuzzy/line matchers, call-graph resolution
crates/aft-store/      Index store formats: segments, family lifecycle, text, content records, call graph, vectors
crates/aft-tokenizer/  Standalone Claude token-counting library
crates/secret-rules/   Secret placeholder grammar, key location rules, response scrubbing
crates/aft-fake-lsp/   Fake language server used by LSP tests
packages/              TypeScript workspace (plugins, bridge, CLI, platform binary packages)
tests/                 Cross-platform end-to-end suites (docker, macos-e2e, windows-e2e, pi-rpc)
test-vectors/          Shared contract vectors read by Rust and TypeScript tests
grammars/              Pinned tree-sitter grammar builds (Luau, SystemVerilog) and their provenance
benchmarks/            Search, retrieval, and compression benchmarks
scripts/               Release, card staging, validation, governed-doc alignment, telemetry
docs/                  User and design documentation (tools, config, CLI, design notes, investigations)
spec/                  Feature-config specification material
spikes/                Throwaway prototypes kept for reference
assets/                Repository images
.github/workflows/     CI, release, and PR gate workflows
```

## `crates/aft/src/`

```text
bin/               Auxiliary binaries (hashline schema artifact generator)
cli/               Native `aft` subcommands (setup, index, warmup, profile, fix-config, backups, cache cleanup, sandbox launch)
commands/          One handler per protocol command; `tool_call.rs` is the agent tool entry
subc/              Subc daemon edge: binds, trust, tool-provider role, remote calls, scopes, code mode, consent, push, drain, health
```

`main.rs` is the process entry point. The tool-call pipeline lives in
`run_tool_call.rs`, `subc_translate.rs`, `subc_format.rs`, `response_finalize.rs`,
`bounded_io_command.rs` and `tool_output.rs`, with submodules in matching
directories. `lib.rs` re-exports the moved modules at their existing `aft::`
paths. `data_layer.rs` is the facade for the engine's `data_layer/`; it also
exposes `trace_measure::run`, whose runner stays in `trace_measure_run.rs`.

`crates/aft/tests/` holds the Rust integration suites, compiled as a few test
binaries (`integration`, `rest`, `engine`, `semantic`, `alert`,
`list_envelope`, `watcher_integration` and some single-purpose ones listed in
`crates/aft/Cargo.toml`), plus `fixtures/` and `helpers/`.

## `crates/aft-engine/src/`

Top-level `.rs` files include `context.rs`, `edit.rs` and `backup.rs`. A
directory with the same name holds the module's submodules (`edit/`, `backup/`,
`sandbox_spawn/`, ...). Other engine directories include:

```text
agent_data/        Session undo and checkpoint data carried through engram's session door
bash_background/   Background, PTY and remote task registry, watchdog, pattern watches, persistence
bash_db_hints/     Schema hints when a database CLI fails on an unknown table or column
bash_permissions/  Permission scan of parsed bash commands
bash_rewrite/      Rewrites simple bash commands into AFT tool calls
data_layer/        AFT's side of the index store: checkouts, runtime pools, incremental call graph, meaning fill, parent index state
exec_remote/       Client for the daemon's remote execution capability (`runon`)
gh_shim_relay/     Daemon-side relay for governed `gh` bot writes
gh_shim_speech/    Launch-bound bot-write authority
github_read/       `issue://` and `pr://` fetch, normalize, render, fallback cache
grammar/           Downloaded-grammar re-export and runtime tests
hashline/          Hashline edit engine: scan, snapshot, syntax, apply, transaction, recovery, oracle
inspect/           `aft_inspect` categories, scanners, Oxc liveness engine, tier-2 scheduler
lsp/               Language-server client, registry, roots, diagnostics, child processes
page_collect/      Page and referenced-file snapshots for sealed HTML page bundles
parent_search/     Parent-folder sessions: child repo discovery, background worker, external search
patch/             `apply_patch` parser, matcher, applier
search_b2/         `aft_search` query router, lane contracts and token variants
search_state/      Search-owned state, candidate memoization, and rerankers shared with the runtime
watcher_backend/   Native watcher backends (FSEvents, inotify, Windows)
watcher_filter/    Bounded inbox between the native backends and the watcher's filter thread
```

Compatibility directories `callgraph_store/`, `imports/`, `executor/`, `db/`,
`secret/`, and `secret_proxy/` re-export the split crates and also hold tests;
their implementations live in the crates named above.

Unit tests that exercise the engine alone live with it. Tests that drive
handlers or the daemon edge live in `crates/aft/src/`; they can reach engine
fixtures and observers through `aft_engine::test_bridge` with the
`test-support` feature. The engine does not depend on `agent-file-tools`.

## `packages/`

```text
aft-bridge/        Shared by all harnesses: transports (standalone pool, subc), binary resolution and download, ONNX runtime, logging, formatting helpers
aft-cli/           `npx @cortexkit/aft`: setup, doctor, index, LSP and filter management, harness adapters
opencode-plugin/   @cortexkit/aft-opencode: V1 plugin and V2 runtime (`src/entry/`), tools, wakes, permissions, TUI
pi-plugin/         @cortexkit/aft-pi: Pi and OMP plugin, tools, commands, dialogs
npm/               One npm package per platform carrying the prebuilt `aft` binary
```

Each package keeps its tests in `src/__tests__/` (`*.test.ts`).

## Naming

Rust command handlers are snake_case files in `crates/aft/src/commands/`;
Rust tests are `*_test.rs` / `*_tests.rs` or live in `crates/aft/tests/`.
TypeScript tool groups are short capability nouns in `packages/*/src/tools/`;
tests are `*.test.ts`.

## Where to add new code

- **Agent tool, argument mapping:** `crates/aft/src/subc_translate.rs`.
- **Agent tool, rendered text:** `crates/aft/src/subc_format.rs`.
- **Agent tool, OpenCode adapter:** `packages/opencode-plugin/src/tools/[group].ts`, registered in `src/tool-registration.ts`. Hoisted host-slot tools go in `tools/hoisted.ts`.
- **Agent tool, Pi adapter:** `packages/pi-plugin/src/tools/[group].ts`, registered in `src/tool-registration.ts`.
- **Agent tool argument added, removed or renamed:** regenerate `crates/aft/src/subc_tool_schemas.json` with `bun scripts/build-tool-schemas.ts` in `packages/opencode-plugin`.
- **Runner catalog (presets, per-tool tags, capabilities):** `crates/aft/src/subc/tool_provider.rs`; the catalog goldens regenerate with `cargo test -p agent-file-tools --lib regenerate_tool_provider_catalog -- --ignored`.
- **Rust command handler:** `crates/aft/src/commands/[name].rs`, exported from `commands/mod.rs` and dispatched from `main.rs`.
- **Management operation (no agent tool):** `crates/aft/src/commands/[operation].rs`, for example `health_digest.rs`.
- **Shared engine logic:** `crates/aft-engine/src/[domain].rs` (or `crates/aft-base/src/[domain].rs` for cross-crate primitives), outside command handlers.
- **Import language:** implement `ImportSyntax` in `crates/aft-parse/src/imports/` and register it in `imports/mod.rs` (`syntax_for`).
- **Bash output compressor:** `crates/aft-base/src/compress/[tool].rs`, implementing `Compressor`; declare it and add it to `compressors_in_dispatch_order()` in `crates/aft-base/src/compress/mod.rs`. For a declarative filter, add `crates/aft-base/src/compress/builtin_filters/[tool].toml` and list it in `builtin_filters.rs`.
- **Bash rewrite rule:** `crates/aft-engine/src/bash_rewrite/rules.rs` (implements `RewriteRule`), with its decision class in `catalog.rs` and dispatch in `dispatch.rs`.
- **Inspect scanner:** `crates/aft-engine/src/inspect/scanners/[scan].rs`, registered in `scanners/mod.rs`.
- **LSP behavior:** `crates/aft-engine/src/lsp/[module].rs`.
- **Patch parsing or matching:** `crates/aft-engine/src/patch/`.
- **Hashline apply or repair rule:** `crates/aft-engine/src/hashline/apply/`. For oracle vectors, use `hashline/oracle/`.
- **Sandbox rules:** `crates/aft-db/src/sandbox_profile.rs` (profile), `crates/aft-engine/src/sandbox_spawn.rs` (spawn policy), `crates/aft/src/cli/sandbox_launch/` (Seatbelt and Landlock backends).
- **Access policy and grants:** `crates/aft-db/src/access_policy.rs`; per-call route policy in `crates/aft/src/subc/call_policy.rs`; remote folder grants and consent in `crates/aft/src/subc/remote_consent.rs` and `crates/aft-db/src/db/remote_grant_rows.rs`.
- **Session trust for a new runner or reserved module:** the exact (module, harness) pair in `HARNESS_SCOPED_FIRST_PARTY_MODULES` (`crates/aft/src/subc/mod.rs`).
- **Agent child environment, git hooks:** `crates/aft-engine/src/agent_child_env.rs`.
- **`gh` shim routing:** `crates/aft-engine/src/gh_shim.rs`. GitHub read rendering: `crates/aft-engine/src/github_read/`.
- **Alerts:** `crates/aft-engine/src/alert_state.rs` (ingest), `crates/aft/src/alert_render.rs` (reminder text), `crates/aft-base/src/alert_records.rs` (SQLite records).
- **List truncation surface:** `crates/aft-base/src/list_surfaces/[surface].rs`, registered in `crates/aft-base/src/list_surfaces.rs`.
- **Semantic embedding backend:** `crates/aft-engine/src/semantic_index.rs`, `crates/aft-engine/src/synapse_embed.rs`, and the backend config in `crates/aft-base/src/config.rs`.
- **Index store format (a section, record or lifecycle rule):** `crates/aft-store/src/`. How AFT builds, refreshes or reads it: `crates/aft-engine/src/data_layer/` (including `text_scope.rs`) and `crates/aft-engine/src/inspect/data_sources.rs`; command read adapters: `crates/aft/src/commands/data_callgraph.rs` and `crates/aft/src/commands/data_search.rs`.
- **Call-graph resolution rule:** `crates/aft-parse/src/callgraph_store/` (dispatch, bindings, facts); bump `EXTRACTOR_VERSION` in `crates/aft-engine/src/data_layer/content.rs` when stored records change.
- **Downloaded grammar:** `crates/aft-parse/src/grammar/` (manifest, download, runtime); language selection in `crates/aft-parse/src/parser/grammar_selection.rs`, engine integration in `crates/aft-engine/src/data_layer/grammar.rs`. Grammar identity also participates in record keys (`data_layer/content.rs`).
- **Standing root keys:** `crates/aft-engine/src/scoped_key.rs` and `crates/aft-db/src/db/standing_roots.rs`.
- **Windows path normalization:** `crates/aft-base/src/windows_path.rs`.
- **Bridge export:** `packages/aft-bridge/src/[module].ts`, re-exported from `src/index.ts`.
- **CLI command:** `packages/aft-cli/src/commands/[command].ts`, dispatched from `src/index.ts`.
- **Platform binary package:** `packages/npm/[platform]/`.
- **Rust integration test:** `crates/aft/tests/integration/`.
- **Benchmark:** `benchmarks/[name]/`.
