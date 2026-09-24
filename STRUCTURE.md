# Codebase Structure

A directory map. How the pieces work together is in
[ARCHITECTURE.md](ARCHITECTURE.md). For individual files, search the code:
it stays current and this document does not list them.

## Top level

```text
crates/aft/            Rust engine: the `aft` binary, command handlers, indexes, integration tests
crates/aft-tokenizer/  Standalone Claude token-counting library
packages/              TypeScript workspace (plugins, bridge, CLI, platform binary packages)
tests/                 Cross-platform end-to-end suites (docker, macos-e2e, windows-e2e, pi-rpc)
benchmarks/            Search, retrieval, and compression benchmarks
scripts/               Release, validation, governed-doc alignment, telemetry, Windows VM helpers
docs/                  User and design documentation (tools, config, CLI, design notes, investigations)
spec/                  Feature-config specification material
spikes/                Throwaway prototypes kept for reference
assets/                Repository images
.github/workflows/     CI, release, and PR gate workflows
```

## `crates/aft/src/`

Top-level `.rs` files are shared engine modules (parser, edit, config,
context, search and semantic indexes, and so on). Subdirectories:

```text
alias/             Git blob-id aliases and byte-exact manifest path identities
bash_background/   Background and PTY task registry, watchdog, pattern watches, persistence
bash_permissions/  Permission scan of parsed bash commands
bash_rewrite/      Rewrites simple bash commands into AFT tool calls
bin/               Auxiliary binaries (hashline schema artifact generator)
blob_store/        Content-addressed per-plane blob storage for index views
callgraph_store/   Persisted SQLite callgraph: build, refresh, queries, dead-code projection
cli/               Native `aft` subcommands (setup, index, warmup, profile, fix-config, sandbox launch)
commands/          One handler per protocol command; `tool_call.rs` is the agent tool entry
compress/          Bash output compressors, TOML filter engine, builtin filters, filter trust
db/                SQLite stores (backups, bash tasks and watches, standing roots, GitHub cache, lifecycle)
executor/          Per-root actor scheduler, lanes, job classes, cancellation
gc/                Mark-and-sweep for view blob stores
github_read/       `issue://` and `pr://` fetch, normalize, render, fallback cache
hashline/          Hashline edit engine: scan, snapshot, syntax, apply, transaction, recovery, oracle
imports/           Per-language import engines for `aft_import`
inspect/           `aft_inspect` categories, scanners, Oxc liveness engine, tier-2 scheduler
list_surfaces/     Truncation envelope adapters for list-shaped results
lsp/               Language-server client, registry, roots, diagnostics, child processes
migrate_storage/   Storage migration log
migration/         One-way import of legacy semantic snapshots into views
patch/             `apply_patch` parser, matcher, applier
path_status/       Pending or failed path annotations for view generations
pins/              Generation pins held while a view is assembled or read
refresh/           Plane-worker refresh coordination for views
search_b2/         `aft_search` query router, lane plans, token variants, readiness
subc/              Subc daemon edge: routes, trust, wire, bash, push, drain, health, standing actor
views/             Per-checkout view manifests and generation publication
watcher/           Project file watcher
watcher_backend/   Native watcher backends (FSEvents, inotify, Windows)
```

`crates/aft/tests/` holds Rust integration suites (`integration/`,
`fixtures/`, `helpers/`, plus top-level contract and bench tests).

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
Rust tests are `*_test.rs` or live in `crates/aft/tests/`. TypeScript tool
groups are short capability nouns in `packages/*/src/tools/`; tests are
`*.test.ts`.

## Where to add new code

- **Agent tool, argument mapping:** `crates/aft/src/subc_translate.rs`.
- **Agent tool, rendered text:** `crates/aft/src/subc_format.rs`.
- **Agent tool, OpenCode adapter:** `packages/opencode-plugin/src/tools/[group].ts`, registered in `src/tool-registration.ts`. Hoisted host-slot tools go in `tools/hoisted.ts`.
- **Agent tool, Pi adapter:** `packages/pi-plugin/src/tools/[group].ts`, registered in `src/tool-registration.ts`.
- **Rust command handler:** `crates/aft/src/commands/[name].rs`, exported from `commands/mod.rs` and dispatched from `main.rs`.
- **Management operation (no agent tool):** `crates/aft/src/commands/[operation].rs`, for example `health_digest.rs`.
- **Shared engine logic:** `crates/aft/src/[domain].rs`, outside command handlers.
- **Import language:** `crates/aft/src/imports/[language].rs`, implementing `ImportSyntax` and registered in `imports/mod.rs`.
- **Bash output compressor:** `crates/aft/src/compress/[tool].rs`, implementing `Compressor` and registered in `compress/mod.rs`. For a declarative filter, add `compress/builtin_filters/[tool].toml` and list it in `builtin_filters.rs`.
- **Bash rewrite rule:** `crates/aft/src/bash_rewrite/rules.rs` (implements `RewriteRule`), with its decision class in `catalog.rs` and dispatch in `dispatch.rs`.
- **Inspect scanner:** `crates/aft/src/inspect/scanners/[scan].rs`, registered in `scanners/mod.rs`.
- **LSP behavior:** `crates/aft/src/lsp/[module].rs`.
- **Patch parsing or matching:** `crates/aft/src/patch/`.
- **Hashline apply or repair rule:** `crates/aft/src/hashline/apply/`. For oracle vectors, use `hashline/oracle/`.
- **Sandbox rules:** `sandbox_profile.rs` (profile), `sandbox_spawn.rs` (spawn policy), `cli/sandbox_launch/` (Seatbelt and Landlock backends).
- **Agent child environment, git hooks:** `crates/aft/src/agent_child_env.rs`.
- **`gh` shim routing:** `crates/aft/src/gh_shim.rs`. GitHub read rendering: `crates/aft/src/github_read/`.
- **Alerts:** `alert_state.rs` (ingest), `alert_render.rs` (reminder text), `alert_records.rs` (SQLite records).
- **List truncation surface:** `crates/aft/src/list_surfaces/[surface].rs`, registered in `list_surfaces.rs`.
- **Build breaker domain:** `crates/aft/src/build_breaker.rs`.
- **Semantic embedding backend:** `crates/aft/src/semantic_index.rs`, `synapse_embed.rs`, and the backend config in `config.rs`.
- **Standing root keys:** `scoped_key.rs` and `db/standing_roots.rs`.
- **Windows path normalization:** `crates/aft/src/windows_path.rs`.
- **Bridge export:** `packages/aft-bridge/src/[module].ts`, re-exported from `src/index.ts`.
- **CLI command:** `packages/aft-cli/src/commands/[command].ts`, dispatched from `src/index.ts`.
- **Platform binary package:** `packages/npm/[platform]/`.
- **Rust integration test:** `crates/aft/tests/integration/`.
- **Benchmark:** `benchmarks/[name]/`.
