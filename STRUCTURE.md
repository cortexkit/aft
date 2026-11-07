# Codebase Structure

A directory map. How the pieces work together is in
[ARCHITECTURE.md](ARCHITECTURE.md). For individual files, search the code:
it stays current and this document does not list them.

## Top level

```text
crates/aft/            Rust engine: the `aft` binary, command handlers, indexes, integration tests
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

Top-level `.rs` files are shared engine modules (parser, edit, config,
context, search and semantic indexes, and so on). A folder with the same name
as a top-level file holds that module's submodules (`edit/`, `backup/`,
`run_tool_call/`, `sandbox_spawn/`, `subc_translate/`, ...). The other folders:

```text
agent_data/        Session undo and checkpoint data carried through engram's session door
alias/             Git blob-id aliases and byte-exact manifest path identities
bash_background/   Background, PTY and remote task registry, watchdog, pattern watches, persistence
bash_db_hints/     Schema hints when a database CLI fails on an unknown table or column
bash_permissions/  Permission scan of parsed bash commands
bash_rewrite/      Rewrites simple bash commands into AFT tool calls
bin/               Auxiliary binaries (hashline schema artifact generator)
blob_store/        Content-addressed per-plane blob storage for index views
callgraph_store/   Persisted SQLite callgraph: build, refresh, queries, dead-code projection
cli/               Native `aft` subcommands (setup, index, warmup, profile, fix-config, backups, cache cleanup, sandbox launch)
commands/          One handler per protocol command; `tool_call.rs` is the agent tool entry
compress/          Bash output compressors, TOML filter engine, builtin filters, filter trust
db/                aft.db stores (backups, bash tasks and watches, call ledger, access grants, GitHub cache, ...)
exec_remote/       Client for the daemon's remote execution capability (`runon`)
executor/          Per-root actor scheduler, lanes, job classes, cancellation
gc/                Mark-and-sweep for view blob stores
gh_shim_relay/     Daemon-side relay for governed `gh` bot writes
gh_shim_speech/    Launch-bound bot-write authority
github_read/       `issue://` and `pr://` fetch, normalize, render, fallback cache
hashline/          Hashline edit engine: scan, snapshot, syntax, apply, transaction, recovery, oracle
imports/           Per-language import engines for `aft_import`
inspect/           `aft_inspect` categories, scanners, Oxc liveness engine, tier-2 scheduler
lsp/               Language-server client, registry, roots, diagnostics, child processes
migration/         One-way import of legacy semantic snapshots into views
page_collect/      Page and referenced-file snapshots for sealed HTML page bundles
patch/             `apply_patch` parser, matcher, applier
path_status/       Pending or failed path annotations for view generations
pins/              Generation pins held while a view is assembled or read
refresh/           Plane-worker refresh coordination for views
search_b2/         `aft_search` query router, lane plans, token variants, readiness
secret/            Secret vault seam and its consent adapter
secret_proxy/      Command-private proxy that releases resolved secrets to one command
subc/              Subc daemon edge: binds, trust, tool-provider role, scopes, code mode, consent, push, drain, health
views/             Per-checkout view manifests and generation publication
watcher/           Project file watcher
watcher_backend/   Native watcher backends (FSEvents, inotify, Windows)
```

`crates/aft/tests/` holds the Rust integration suites, compiled as a few test
binaries (`integration`, `rest`, `engine`, `semantic`, `alert`,
`list_envelope`, `watcher_integration` and some single-purpose ones listed in
`crates/aft/Cargo.toml`), plus `fixtures/` and `helpers/`.

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
- **Shared engine logic:** `crates/aft/src/[domain].rs`, outside command handlers.
- **Import language:** `crates/aft/src/imports/[language].rs`, implementing `ImportSyntax` and registered in `imports/mod.rs`.
- **Bash output compressor:** `crates/aft/src/compress/[tool].rs`, implementing `Compressor` and registered in `compress/mod.rs`. For a declarative filter, add `compress/builtin_filters/[tool].toml` and list it in `builtin_filters.rs`.
- **Bash rewrite rule:** `crates/aft/src/bash_rewrite/rules.rs` (implements `RewriteRule`), with its decision class in `catalog.rs` and dispatch in `dispatch.rs`.
- **Inspect scanner:** `crates/aft/src/inspect/scanners/[scan].rs`, registered in `scanners/mod.rs`.
- **LSP behavior:** `crates/aft/src/lsp/[module].rs`.
- **Patch parsing or matching:** `crates/aft/src/patch/`.
- **Hashline apply or repair rule:** `crates/aft/src/hashline/apply/`. For oracle vectors, use `hashline/oracle/`.
- **Sandbox rules:** `sandbox_profile.rs` (profile), `sandbox_spawn.rs` (spawn policy), `cli/sandbox_launch/` (Seatbelt and Landlock backends).
- **Access policy and grants:** `crates/aft/src/access_policy.rs`; per-call route policy in `subc/call_policy.rs`.
- **Session trust for a new runner or reserved module:** the exact (module, harness) pair in `HARNESS_SCOPED_FIRST_PARTY_MODULES` (`subc/mod.rs`).
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
