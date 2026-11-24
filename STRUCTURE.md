# Codebase Structure

A directory map. How the pieces work together is in
[ARCHITECTURE.md](ARCHITECTURE.md). For individual files, search the code:
it stays current and this document does not list them.

## Top level

```text
crates/aft/            `agent-file-tools`: the `aft` binary and library facade, auxiliary binaries, examples, integration tests
crates/aft-tools/      `aft-tools`: native CLI, tool-call pipeline, and command handlers
crates/aft-subc/       `aft-subc`: Subc daemon integration and tool catalog assets
crates/aft-engine/     Runtime composition: project contexts, lifecycle, coordination and compatibility paths
crates/aft-edit/       Edits, formatting, hashline, imports, patches, backups and checkpoints
crates/aft-index/      Search, indexes, embeddings, runtime grammars and file watchers
crates/aft-inspect/    Inspection, language servers and alert state
crates/aft-github/     GitHub reads, the gh shim and the worker broker client and protocol
crates/aft-shell/      Shell tasks, sandbox spawning, remote execution and worker broker provisioning
crates/aft-base/       Shared foundation: protocol, config, executor, bash compression, secrets and locks
crates/aft-db/         SQLite stores (`aft.db`), access policy, sandbox profile, write ledger
crates/aft-parse/      Syntax parsing, grammars, imports, fuzzy/line matchers, call-graph resolution
crates/aft-store/      Index store formats: segments, family lifecycle, text, content records, call graph, vectors
crates/aft-tokenizer/  Standalone Claude token-counting library
crates/secret-rules/   Secret placeholder grammar, key location rules, response scrubbing
crates/aft-fake-lsp/   Fake language server used by LSP tests
packages/              TypeScript workspace (plugins, bridge, CLI, platform binary packages)
tests/                 Cross-platform end-to-end suites (docker, macos-e2e, windows-e2e, pi-rpc)
test-vectors/          Shared contract vectors read by Rust and TypeScript tests
grammars/              Pinned tree-sitter grammar builds (Luau) and their provenance
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
lib.rs             Re-exports the tools, daemon, and lower-crate modules at their existing aft:: paths
main.rs            Process entry point
```

`crates/aft/tests/` holds the Rust integration suites, compiled as a few test
binaries (`integration`, `rest`, `engine`, `semantic`, `alert`,
`list_envelope`, `watcher_integration` and some single-purpose ones listed in
`crates/aft/Cargo.toml`), plus `fixtures/` and `helpers/`. The
`aft-test-scope-owner` binary is in `crates/aft/tests/helpers/scope_owner.rs`;
example targets are in `crates/aft/examples/`.

## `crates/aft-tools/src/` and `crates/aft-subc/src/`

`aft-tools` (library `aft_tools`) owns `cli/` (native subcommands) and
`commands/` (protocol command handlers, with `tool_call.rs` as the agent tool
entry). The tool-call pipeline shared by standalone and daemon mode lives in
`run_tool_call.rs`, `subc_translate.rs`, `subc_format.rs`,
`response_finalize.rs`, `bounded_io_command.rs` and `tool_output.rs`.
`data_layer.rs` adapts the runtime's index layer and exposes the
`trace_measure::run` runner in `trace_measure_run.rs`.

`aft-subc` (library `aft_subc`) owns `subc/`: daemon binds, trust, tool-provider
registration, remote calls, scopes, code mode, consent, push notifications,
graceful shutdown, and health checks. It embeds the
[tool schemas](crates/aft-subc/src/subc_tool_schemas.json) and
[tool presets](crates/aft-subc/src/subc_tool_presets.json).
`aft-subc` depends on `aft-tools`, not the reverse. Daemon tests that need
handler fixtures use `aft-tools/test-support`. See the
[boundary inventory](crates/aft-tools/BOUNDARY.md) for shared interfaces and test
fixtures.

## `crates/aft-engine/src/`

`context.rs` and `context/` hold `AppContext` and its coordination logic.
Other composition modules cover configuration reloads, database opening,
runtime status and shutdown, parent-folder sessions (`parent_search/`), and
page snapshots (`page_collect/`). The engine also wires bash rewrites into the
context and coordinates the shell change journal with file watchers.

Re-exports and adapters preserve `aft_engine::` paths. Compatibility
directories such as `callgraph_store/`, `db/`, `executor/` and `secret/` also
keep tests. See the [engine boundary table](crates/aft-engine/BOUNDARY.md) for
the owner and purpose of each port, compatibility adapter and other
cross-crate item.

Composed behavior and source guards are tested in the engine. Upper-crate
tests reach engine fixtures through `aft_engine::test_bridge` with the
`test-support` feature. The engine does not depend on the facade, tools, or
daemon crates.

## Area crates

Area implementations and their unit tests live below the engine, in:

| Source directory | Main modules |
|---|---|
| `crates/aft-edit/src/` | `edit/`, `hashline/`, `patch/`, `imports/`, `agent_data/` and `backup/` |
| `crates/aft-index/src/` | `data_layer/` for the index store, `search_b2/` and `search_state/` for search, embeddings, `grammar/` and `watcher_backend/` |
| `crates/aft-inspect/src/` | `inspect/` for scanners and background diagnostic refresh, `lsp/` for language-server processes and diagnostics, `alert_state.rs` |
| `crates/aft-github/src/` | `github_read/`, `gh_shim/`, `gh_shim_relay/`, `gh_shim_speech/` and the broker client in `gh_shim_worker_broker.rs` |
| `crates/aft-shell/src/` | `bash_background/`, `bash_rewrite/`, `bash_permissions/`, `sandbox_spawn/`, `exec_remote/` and broker provisioning in `gh_shim_worker_broker.rs` |

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

Rust command handlers are snake_case files in `crates/aft-tools/src/commands/`;
Rust tests are `*_test.rs` / `*_tests.rs` or live in `crates/aft/tests/`.
TypeScript tool groups are short capability nouns in `packages/*/src/tools/`;
tests are `*.test.ts`.

## Where to add new code

- **Agent tool, argument mapping:** `crates/aft-tools/src/subc_translate.rs`.
- **Agent tool, rendered text:** `crates/aft-tools/src/subc_format.rs`.
- **Agent tool, OpenCode adapter:** `packages/opencode-plugin/src/tools/[group].ts`, registered in `src/tool-registration.ts`. Hoisted host-slot tools go in `tools/hoisted.ts`.
- **Agent tool, Pi adapter:** `packages/pi-plugin/src/tools/[group].ts`, registered in `src/tool-registration.ts`.
- **Agent tool argument added, removed or renamed:** regenerate `crates/aft-subc/src/subc_tool_schemas.json` with `bun run --cwd packages/opencode-plugin build:tool-schemas`.
- **Runner catalog (presets, per-tool tags, capabilities):** `crates/aft-subc/src/subc/tool_provider.rs`; the catalog goldens regenerate with `cargo test -p aft-subc --lib regenerate_tool_provider_catalog -- --ignored`.
- **Rust command handler:** `crates/aft-tools/src/commands/[name].rs`, exported from `commands/mod.rs` and dispatched from `main.rs`.
- **Management operation (no agent tool):** `crates/aft-tools/src/commands/[operation].rs`, for example `health_digest.rs`.
- **Shared runtime logic:** put area behavior in the area crate, context coordination in `crates/aft-engine/src/`, and shared primitives in `crates/aft-base/src/`, outside command handlers.
- **Import language:** implement `ImportSyntax` in `crates/aft-parse/src/imports/` and register it in `imports/mod.rs` (`syntax_for`).
- **Bash output compressor:** `crates/aft-base/src/compress/[tool].rs`, implementing `Compressor`; declare it and add it to `compressors_in_dispatch_order()` in `crates/aft-base/src/compress/mod.rs`. For a declarative filter, add `crates/aft-base/src/compress/builtin_filters/[tool].toml` and list it in `builtin_filters.rs`.
- **Bash rewrite rule:** `crates/aft-shell/src/bash_rewrite/rules.rs` (implements `RewriteRule`), with its decision class in `catalog.rs`; context dispatch lives in `crates/aft-engine/src/bash_rewrite/dispatch.rs`.
- **Inspect scanner:** `crates/aft-inspect/src/inspect/scanners/[scan].rs`, registered in `scanners/mod.rs`.
- **LSP behavior:** `crates/aft-inspect/src/lsp/[module].rs`.
- **Patch parsing or matching:** `crates/aft-edit/src/patch/`.
- **Hashline apply or repair rule:** `crates/aft-edit/src/hashline/apply/`. Reference input/output fixtures live in `hashline/oracle/`.
- **Sandbox rules:** `crates/aft-db/src/sandbox_profile.rs` (profile), `crates/aft-shell/src/sandbox_spawn.rs` (spawn policy), `crates/aft-tools/src/cli/sandbox_launch/` (Seatbelt and Landlock backends).
- **Access policy and grants:** `crates/aft-db/src/access_policy.rs`; per-call route policy in `crates/aft-subc/src/subc/call_policy.rs`; remote folder grants and consent in `crates/aft-subc/src/subc/remote_consent.rs` and `crates/aft-db/src/db/remote_grant_rows.rs`.
- **Session trust for a new runner or reserved module:** the exact (module, harness) pair in `HARNESS_SCOPED_FIRST_PARTY_MODULES` (`crates/aft-subc/src/subc/mod.rs`).
- **Agent child environment, git hooks:** `crates/aft-shell/src/agent_child_env.rs`.
- **`gh` shim routing:** `crates/aft-github/src/gh_shim.rs`. GitHub read rendering: `crates/aft-github/src/github_read/`.
- **Alerts:** `crates/aft-inspect/src/alert_state.rs` (ingest), `crates/aft-tools/src/alert_render.rs` (reminder text), `crates/aft-base/src/alert_records.rs` (SQLite records).
- **List truncation surface:** `crates/aft-base/src/list_surfaces/[surface].rs`, registered in `crates/aft-base/src/list_surfaces.rs`.
- **Build breaker domain:** `crates/aft-db/src/build_breaker.rs`.
- **Semantic embedding backend:** `crates/aft-index/src/semantic_index.rs`, `crates/aft-index/src/synapse_embed.rs`, and the backend config in `crates/aft-base/src/config.rs`.
- **Index store format (a section, record or lifecycle rule):** `crates/aft-store/src/`. How AFT builds, refreshes or reads it: `crates/aft-index/src/data_layer/` and `crates/aft-inspect/src/inspect/data_sources.rs`; command read adapters: `crates/aft-tools/src/commands/data_callgraph.rs` and `crates/aft-tools/src/commands/data_search.rs`.
- **Call-graph resolution rule:** `crates/aft-parse/src/callgraph_store/` (dispatch, bindings, facts); bump `EXTRACTOR_VERSION` in `crates/aft-index/src/data_layer/content.rs` when stored records change.
- **Downloaded grammar:** `crates/aft-parse/src/grammar/` (manifest, download, runtime); language selection in `crates/aft-parse/src/parser/grammar_selection.rs`, index integration in `crates/aft-index/src/data_layer/grammar.rs`. Grammar identity also participates in record keys (`data_layer/content.rs`).
- **Standing root keys:** `crates/aft-db/src/scoped_key.rs` and `crates/aft-db/src/db/standing_roots.rs`.
- **Windows path normalization:** `crates/aft-base/src/windows_path.rs`.
- **Bridge export:** `packages/aft-bridge/src/[module].ts`, re-exported from `src/index.ts`.
- **CLI command:** `packages/aft-cli/src/commands/[command].ts`, dispatched from `src/index.ts`.
- **Platform binary package:** `packages/npm/[platform]/`.
- **Rust integration test:** `crates/aft/tests/integration/`.
- **Benchmark:** `benchmarks/[name]/`.
