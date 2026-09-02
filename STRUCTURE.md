# Codebase Structure

## Directory Layout

```text
opencode-aft/
├── crates/                    # Rust workspace packages
│   ├── aft/                   # Core AFT library, CLI binary, command handlers, and tests
│   └── aft-tokenizer/         # Tokenizer library for Claude API token counting
├── packages/                  # JavaScript workspace packages
│   ├── aft-bridge/            # Shared transport, binary resolution, ONNX runtime helpers
│   ├── aft-cli/               # Unified CLI (setup, doctor, LSP management)
│   ├── opencode-plugin/       # OpenCode plugin (@cortexkit/aft-opencode)
│   ├── pi-plugin/             # Pi coding-agent plugin (@cortexkit/aft-pi)
│   └── npm/                   # Platform-specific npm binary packages
├── tests/                     # Cross-platform test infrastructure
│   ├── docker/                # Docker-based end-to-end tests (Linux) and interactive setup sandbox
│   ├── macos-e2e/             # macOS end-to-end tests
│   ├── pi-rpc/                # Pi RPC protocol tests
│   └── windows-e2e/           # Windows end-to-end tests
├── benchmarks/                # Performance benchmarks (search, compression, retrieval)
├── scripts/                   # Release, validation, and version-management scripts
├── docs/                      # User-facing documentation
├── assets/                    # Repository assets (banner image, etc.)
├── .github/workflows/         # CI and release automation workflows
├── Cargo.toml                 # Rust workspace manifest
├── package.json               # JavaScript workspace manifest
├── ARCHITECTURE.md            # Architecture documentation
├── STRUCTURE.md               # This file
└── README.md                  # User-facing product and tool reference
```

## Directory Purposes

**`crates/aft/`:**
- Purpose: Keep the Rust execution engine, stdin/stdout protocol binary, and shared analysis logic together.
- Contains: `src/` Rust modules, `tests/` integration suites, `crates/aft/tests/fixtures/` test fixtures, `tests/helpers/` test utilities, `crates/aft/tests/lsp/` LSP integration tests
- Key files: `crates/aft/src/main.rs`, `crates/aft/src/lib.rs`, `crates/aft/src/run_tool_call.rs`, `crates/aft/src/runtime_drain.rs`, `crates/aft/src/subc_translate.rs`, `crates/aft/src/subc_format.rs`, `crates/aft/src/subc/mod.rs`, `crates/aft/src/subc/`, `crates/aft/src/fleet_status.rs`, `crates/aft/src/grep_executor.rs`, `crates/aft/src/calls.rs`, `crates/aft/src/memory.rs`, `crates/aft/src/logging.rs`, `crates/aft/src/gh_shim.rs`, `crates/aft/src/github_read/mod.rs`, `crates/aft/src/build_breaker.rs`, `crates/aft/src/agent_child_env.rs`, `crates/aft/src/scoped_key.rs`, `crates/aft/src/synapse_embed.rs`, `crates/aft/src/walk_boundary.rs`, `crates/aft/src/commands/`, `crates/aft/src/compress/`, `crates/aft/src/imports/`, `crates/aft/src/inspect/`, `crates/aft/src/hashline/`, `crates/aft/src/executor/`, `crates/aft/src/bash_background/`, `crates/aft/src/bash_rewrite/`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, `crates/aft/src/cache_freshness.rs`, `crates/aft/src/fs_lock.rs`, `crates/aft/src/legacy_partitions.rs`, `crates/aft/src/cold_build_limiter.rs`, `crates/aft/src/sandbox_spawn.rs`, `crates/aft/src/sandbox_profile.rs`, `crates/aft/src/cli/sandbox_launch.rs`, `crates/aft/src/symbol_diff.rs`, `crates/aft/src/alert_records.rs`, `crates/aft/src/alert_render.rs`, `crates/aft/src/alert_state.rs`, `crates/aft/tests/integration/`

**`crates/aft-tokenizer/`:**
- Purpose: Ship a standalone tokenizer for Claude API token counting.
- Contains: `src/` Rust source, `benches/` benchmarks, `tests/` tests, `examples/`
- Key files: `crates/aft-tokenizer/src/lib.rs`, `crates/aft-tokenizer/src/claude.rs`

**`crates/aft/src/callgraph_store/`:**
- Purpose: Build and maintain the workspace-wide SQLite database of call dependencies.
- Contains: Generation-based SQLite store builders, watchers, table schemas, queries, and dead code projections.
- Key files: `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/callgraph_store/dead_code_projection.rs`

**`crates/aft/src/commands/`:**
- Purpose: Add one handler file per protocol command.
- Contains: ~66 command-specific request parsing and response generation modules
- Key files: `crates/aft/src/commands/tool_call.rs`, `crates/aft/src/commands/read.rs`, `crates/aft/src/commands/write.rs`, `crates/aft/src/commands/hashline.rs`, `crates/aft/src/commands/apply_patch.rs`, `crates/aft/src/commands/bash_orchestrate.rs`, `crates/aft/src/commands/bash_wait_detach.rs`, `crates/aft/src/commands/outline.rs`, `crates/aft/src/commands/zoom.rs`, `crates/aft/src/commands/bash.rs`, `crates/aft/src/commands/grep.rs`, `crates/aft/src/commands/semantic_search.rs`, `crates/aft/src/commands/configure.rs`, `crates/aft/src/commands/health_digest.rs`

**`crates/aft/src/compress/`:**
- Purpose: Provide tiered output compression for hoisted bash commands.
- Contains: Rust `Compressor` modules per tool (git, cargo, eslint, etc.), declarative TOML filter pipeline, trust model for project filters, builtin filter definitions (22 .toml files)
- Key files: `crates/aft/src/compress/mod.rs`, `crates/aft/src/compress/git.rs`, `crates/aft/src/compress/toml_filter.rs`, `crates/aft/src/compress/trust.rs`, `crates/aft/src/compress/builtin_filters.rs`

**`crates/aft/src/imports/`:**
- Purpose: Host per-language import engines for `aft_import` commands.
- Contains: Language-specific import parsing, add, remove, and organize logic
- Key files: `crates/aft/src/imports/mod.rs`, `crates/aft/src/imports/java.rs`, `crates/aft/src/imports/csharp.rs`, `crates/aft/src/imports/php.rs`, `crates/aft/src/imports/kotlin.rs`, `crates/aft/src/imports/scala.rs`, `crates/aft/src/imports/swift.rs`, `crates/aft/src/imports/ruby.rs`, `crates/aft/src/imports/lua.rs`, `crates/aft/src/imports/c.rs`, `crates/aft/src/imports/perl.rs`

**`crates/aft/src/inspect/`:**
- Purpose: Provide codebase-health scanning (dead code, unused exports, duplicates, import cycles, metrics, TODOs, LSP diagnostics, and framework route/decorator entry points, down-ranking low-value or generated files).
- Contains: Scanner modules for each category, entry point/decorator spec resolvers, generated file filters, signal tiering helpers, and the AST-based liveness graph.
- Key files: `crates/aft/src/inspect/scanners/dead_code.rs`, `crates/aft/src/inspect/scanners/unused_exports.rs`, `crates/aft/src/inspect/scanners/duplicates.rs`, `crates/aft/src/inspect/scanners/cycles.rs`, `crates/aft/src/inspect/scanners/metrics.rs`, `crates/aft/src/inspect/scanners/todos.rs`, `crates/aft/src/inspect/entry_points.rs`, `crates/aft/src/inspect/frameworks.rs`, `crates/aft/src/inspect/generated.rs`, `crates/aft/src/inspect/job.rs`, `crates/aft/src/inspect/oxc_engine/graph.rs`, `crates/aft/src/inspect/tier2_scheduler.rs`, `crates/aft/src/inspect/phase_log.rs`

**`crates/aft/src/lsp/`:**
- Purpose: Keep LSP client, transport, registry, workspace root resolution, child process lifecycle, and diagnostics state separate from command handlers.
- Contains: LSP lifecycle modules, nested virtualenv Python LSP discovery and workspace ladder resolution, workspace root discovery (deduplicating analyzer roots to Cargo workspace manifests), child process registry (with sibling `.reclaimed` worktree reaping), and supporting types
- Key files: `crates/aft/src/lsp/manager.rs`, `crates/aft/src/lsp/client.rs`, `crates/aft/src/lsp/diagnostics.rs`, `crates/aft/src/lsp/roots.rs`, `crates/aft/src/lsp/child_registry.rs`

**`crates/aft/src/executor/`:**
- Purpose: Orchestrate bounded background maintenance and interactive tool queues across project-root actors.
- Contains: Process-wide and per-actor capacity accounting, interactive and maintenance job classes, reader-first admission, deadline-aware writer promotion, queue-deadline pruning, deficit round-robin actor scheduling, worker lanes, dispatch telemetry, and cooperative cancellation tokens.
- Key files: `crates/aft/src/executor/mod.rs`, `crates/aft/src/executor/tests.rs`

**Standing-root scheduling and resource control:**
- Purpose: Share cold-build slots fairly across standing roots without making a developer laptop unresponsive.
- Contains: Process-wide deficit round-robin root scheduling, balanced and performance resource policies, pressure sampling with hysteresis, durable slice coordination, and cross-platform background thread priority control.
- Key files: `crates/aft/src/standing_scheduler.rs`, `crates/aft/src/resource_policy.rs`, `crates/aft/src/subc/standing.rs`, `crates/aft/src/thread_priority.rs`

**`crates/aft/src/bash_background/`:**
- Purpose: Manage background bash tasks, PTY sessions, async pattern watches, and output compression.
- Contains: Process pool, PTY runtime, watchdog thread, persistence, restart fate preservation (`FateUnknown`), process start-time liveness checks, buffer management, async pattern watches
- Key files: `crates/aft/src/bash_background/registry.rs`, `crates/aft/src/bash_background/process.rs`, `crates/aft/src/bash_background/pty_process.rs`, `crates/aft/src/bash_background/watchdog.rs`, `crates/aft/src/bash_background/watches.rs`

**`crates/aft/src/bash_rewrite/`:**
- Purpose: Execute bash command rewriting, rule branch evaluations, differential test campaigns, and observation logging.
- Contains: Rewrite dispatch, decision catalog, rule implementations, differential testing engine, observation logger, command parser, and output footers.
- Key files: `crates/aft/src/bash_rewrite/mod.rs`, `crates/aft/src/bash_rewrite/dispatch.rs`, `crates/aft/src/bash_rewrite/catalog.rs`, `crates/aft/src/bash_rewrite/rules.rs`, `crates/aft/src/bash_rewrite/differential.rs`, `crates/aft/src/bash_rewrite/observation.rs`

**`crates/aft/src/db/`:**
- Purpose: Provide persistent SQLite-backed storage for backups, bash tasks, pattern watches, compression events, state, standing roots, durable build breaker rows, and fallback GitHub read cache entries.
- Contains: Database modules for each storage domain
- Key files: `crates/aft/src/db/mod.rs`, `crates/aft/src/db/backups.rs`, `crates/aft/src/db/bash_tasks.rs`, `crates/aft/src/db/bash_watches.rs`, `crates/aft/src/db/compression_events.rs`, `crates/aft/src/db/standing_roots.rs`, `crates/aft/src/db/github_read_cache.rs`, `crates/aft/src/db/state.rs`

**`crates/aft/src/patch/`:**
- Purpose: Implement patch parsing, sequence matching, fuzzy hunk matching, and update execution.
- Contains: Mod, parser, sequence matcher, and update chunk appliers
- Key files: `crates/aft/src/patch/mod.rs`, `crates/aft/src/patch/parser.rs`, `crates/aft/src/patch/matcher.rs`, `crates/aft/src/patch/apply.rs`

**`crates/aft/src/hashline/`:**
- Purpose: Provide byte scanning, line-tag snapshot stores, parser verification, apply repair, two-phase transactions, remap recovery, session registration, release performance gates, and seed-zero xxHash32 oracle calculation for hashline editing.
- Contains: Byte scanner (`scan/`), snapshot rendering store (`snapshot/`), syntax parser and address verifier (`syntax/`), PUT/CUT/REM apply, same-path section composition, boundary/indent repair, and register store (`apply/`), Phase 1/2 transaction engine with rollback protection (`transaction/`), exact-verbatim remap recovery (`recovery/`), session binding and transport integration (`integration/`), release gates and performance ceilings (`release/`), and pure-Rust xxHash32 digest calculator and oracle parity fixtures (`oracle/`).
- Key files: `crates/aft/src/hashline/mod.rs`, `crates/aft/src/hashline/scan/mod.rs`, `crates/aft/src/hashline/snapshot/mod.rs`, `crates/aft/src/hashline/syntax/mod.rs`, `crates/aft/src/hashline/apply/mod.rs`, `crates/aft/src/hashline/transaction/mod.rs`, `crates/aft/src/hashline/recovery/mod.rs`, `crates/aft/src/hashline/integration/mod.rs`, `crates/aft/src/hashline/release/mod.rs`, `crates/aft/src/hashline/oracle/mod.rs`

**`crates/aft/src/subc/`:**
- Purpose: Connect to and authenticate with the subconscious daemon.
- Contains: TCP loopback client loop, HMAC handshakes, message routing, session route epochs, unbound quiescing, and health reporting.
- Key files: `crates/aft/src/subc/mod.rs`, `crates/aft/src/subc/health.rs`, `crates/aft/src/subc/wire.rs`, `crates/aft/src/subc/bash.rs`

**`crates/aft/src/github_read/`:**
- Purpose: Host structured, cached GitHub issue and pull request read engine.
- Contains: Resource and URL parsing, discussion ordinal comment selector filtering (`/comments/<selector>`), structured `gh` CLI fetches, bot comment compression (`bot_compress.rs`), markdown normalization, canonical rendering, image attachment extraction for vision-capable sessions, and SQLite fallback caching.
- Key files: `crates/aft/src/github_read/mod.rs`, `crates/aft/src/github_read/bot_compress.rs`, `crates/aft/src/github_read/cache.rs`, `crates/aft/src/github_read/fetch.rs`, `crates/aft/src/github_read/render.rs`, `crates/aft/src/github_read/attachments.rs`, `crates/aft/src/github_read/normalize.rs`, `crates/aft/src/github_read/resource.rs`

**`packages/aft-bridge/`:**
- Purpose: Ship the shared bridge transport layer used by both OpenCode and Pi plugins.
- Contains: Transport factory routing selection (via user-tier `subc.connection_file`), subc client connection pooling, session lifecycle records caching (`SessionRecord` wrapping route entry and bg subscriptions), per-realm subc lifecycle management (`lifecycle-registry.ts`), consolidated cache root resolution (`cache-paths.ts`), background event subscriptions, revivable transport pool wrapping, durable logging, bridge lifecycle management, binary resolution, download, npm executable resolution and spawn environment augmentation for PATH-stripped GUI launches (`npm-resolver.ts`), ONNX runtime detection, storage migration, compact formatting, zoom-format rendering, canonical path alias resolution, and host-neutral error adaptation
- Key files: `packages/aft-bridge/src/bridge.ts`, `packages/aft-bridge/src/pool.ts`, `packages/aft-bridge/src/subc-transport.ts`, `packages/aft-bridge/src/revivable-transport.ts`, `packages/aft-bridge/src/transport.ts`, `packages/aft-bridge/src/transport-factory.ts`, `packages/aft-bridge/src/durable-log.ts`, `packages/aft-bridge/src/resolver.ts`, `packages/aft-bridge/src/downloader.ts`, `packages/aft-bridge/src/npm-resolver.ts`, `packages/aft-bridge/src/onnx-runtime.ts`, `packages/aft-bridge/src/migration.ts`, `packages/aft-bridge/src/path-aliases.ts`, `packages/aft-bridge/src/error-contract.ts`, `packages/aft-bridge/src/cache-paths.ts`, `packages/aft-bridge/src/lifecycle-registry.ts`, `packages/aft-bridge/src/bash-host-fallback.ts`

**`packages/aft-cli/`:**
- Purpose: Provide a unified `npx @cortexkit/aft` CLI entry point for setup, doctor, and LSP management across all harnesses.
- Contains: CLI command modules, harness adapter auto-detection (OpenCode/Pi)
- Key files: `packages/aft-cli/src/index.ts`, `packages/aft-cli/src/commands/setup.ts`, `packages/aft-cli/src/commands/doctor.ts`, `packages/aft-cli/src/commands/lsp.ts`, `packages/aft-cli/src/adapters/`

**`packages/opencode-plugin/`:**
- Purpose: Ship the OpenCode-facing package that resolves the binary and registers tools.
- Contains: `src/` TypeScript sources, `src/tools/` tool definitions, `src/shared/` shared utilities, `src/hooks/` lifecycle hooks, `src/tui/` TUI plugin, `__tests__/` unit and e2e tests, package manifest
- Key files: `packages/opencode-plugin/src/index.ts`, `packages/opencode-plugin/src/config.ts`, `packages/opencode-plugin/src/tool-registration.ts`, `packages/opencode-plugin/package.json`

**`packages/opencode-plugin/src/tools/`:**
- Purpose: Group OpenCode tool definitions by capability area.
- Contains: Thin adapters for hoisted (advertising `filePath` on OpenCode for read/write/edit to honor host UI header display contract), reading, import, navigation, refactor, safety, bash, conflict, AST, search, semantic, and inspect tools; permissions and internals helpers
- Key files: `packages/opencode-plugin/src/tools/_shared.ts`, `packages/opencode-plugin/src/tools/hoisted.ts`, `packages/opencode-plugin/src/tools/reading.ts`, `packages/opencode-plugin/src/tools/refactoring.ts`, `packages/opencode-plugin/src/tools/bash.ts`, `packages/opencode-plugin/src/tools/inspect.ts`, `packages/opencode-plugin/src/tools/search.ts`

**`packages/pi-plugin/`:**
- Purpose: Ship the Pi coding-agent facing package that resolves the binary and registers tools.
- Contains: `src/` TypeScript sources, `src/tools/` tool definitions, `src/commands/` Pi-specific commands, `src/dialogs/` Pi dialog handlers, `src/shared/` shared utilities, `__tests__/` unit and e2e tests
- Key files: `packages/pi-plugin/src/index.ts`, `packages/pi-plugin/src/config.ts`, `packages/pi-plugin/src/tool-registration.ts`, `packages/pi-plugin/src/types.ts`, `packages/pi-plugin/src/tools/hoisted.ts`

**`packages/pi-plugin/src/tools/`:**
- Purpose: Group Pi tool definitions by capability area.
- Contains: Thin adapters for hoisted, reading, import, navigation, refactor, safety, bash, conflict, AST, semantic, and inspect tools; custom renderers honoring Pi result expansion (`RenderResultOptionsLike`), render helpers, diff-format helper
- Key files: `packages/pi-plugin/src/tools/_shared.ts`, `packages/pi-plugin/src/tools/hoisted.ts`, `packages/pi-plugin/src/tools/reading.ts`, `packages/pi-plugin/src/tools/imports.ts`, `packages/pi-plugin/src/tools/navigate.ts`, `packages/pi-plugin/src/tools/refactor.ts`, `packages/pi-plugin/src/tools/fs.ts`, `packages/pi-plugin/src/tools/render-helpers.ts`

**`packages/npm/`:**
- Purpose: Publish one npm package per target platform so the plugin can resolve a bundled binary.
- Contains: Per-platform package manifests and `bin/` payload directories
- Key files: `packages/npm/darwin-arm64/package.json`, `packages/npm/darwin-x64/package.json`, `packages/npm/linux-arm64/package.json`, `packages/npm/linux-x64/package.json`, `packages/npm/win32-arm64/package.json`, `packages/npm/win32-x64/package.json`

**`benchmarks/`:**
- Purpose: Run benchmark scenarios for search, compression, and retrieval performance.
- Contains: Benchmark source files, configs, cached results, corpora data, package manifests, and trigram index A/B latency comparison tools.
- Key subdirectories: `benchmarks/src/`, `benchmarks/aft-search/`, `benchmarks/codegraph-replication/`, `benchmarks/codegraph-vs-aft-agent/`, `benchmarks/codegraph-vs-aft-retrieval/`, `benchmarks/compression-tokens/`
- Key files: `benchmarks/trigram-ab-latency.py`, `benchmarks/GREP_GLOB_LATENCY.md`, `benchmarks/grep-glob-vs-rg.py`

**`scripts/`:**
- Purpose: Automate release, validation, and version synchronization tasks.
- Contains: Shell and Node scripts, Windows VM helpers
- Key files: `scripts/release.sh`, `scripts/version-sync.mjs`, `scripts/validate-packages.mjs`, `scripts/align-governed-docs.sh`, `scripts/windows-vm/`

**`tests/`:**
- Purpose: Host cross-platform end-to-end test suites.
- Contains: Docker-based Linux e2e tests, macOS e2e tests, Pi RPC protocol tests, Windows e2e tests, and interactive setup/doctor sandboxes.
- Key files: `tests/docker/fixtures/`, `tests/macos-e2e/`, `tests/pi-rpc/`, `tests/windows-e2e/`, `tests/docker/Dockerfile.setup-sandbox`

**`crates/aft/tests/`:**
- Purpose: Host Rust integration tests and test infrastructure.
- Contains: `integration/` test suites, `fixtures/` test data (callgraph, extract_function, inline_symbol, move_symbol), `helpers/` test utilities, `lsp/` LSP-specific tests, top-level test files (semantic, compress)
- Key files: `crates/aft/tests/integration/`, `crates/aft/tests/fixtures/`, `crates/aft/tests/semantic_test.rs`

## Key File Locations

**Entry Points:** `packages/opencode-plugin/src/index.ts` -- register OpenCode plugin tools; `packages/pi-plugin/src/index.ts` -- register Pi plugin tools; `packages/aft-cli/src/index.ts` -- unified CLI dispatcher; `packages/aft-bridge/src/index.ts` -- shared bridge module exports; `crates/aft/src/main.rs` -- start the Rust request loop; `crates/aft/src/gh_shim.rs` -- handle credential-free `gh` routing shim invocations; `crates/aft/src/cli/` -- warmup and storage-migration subcommands; `crates/aft/src/subc/mod.rs` -- handle subc loopback daemon connection and routing; `.github/workflows/release.yml` -- drive tagged release publishing.

**Configuration:** `package.json` -- define Bun workspace scripts; `Cargo.toml` -- define the Rust workspace; `packages/opencode-plugin/src/config.ts` -- parse user and project AFT config for OpenCode; `packages/pi-plugin/src/config.ts` -- parse user and project AFT config for Pi; `crates/aft/src/config.rs` -- parse the shared Rust-side config (semantic backend, LSP servers, bash compression, user-tier `gh_read` gate, etc.). User-level AFT settings reside in the unified CortexKit location `~/.config/cortexkit/aft.jsonc`, and project-level overrides reside in `<project_root>/.cortexkit/aft.jsonc`. The master toggle `"enabled": false` (configured globally or per-project) disables plugin loading and AFT execution.

**Core Logic:** `crates/aft/src/parser.rs` -- extract symbols and languages using thread-local parsers and query-free Rust symbol extraction; `crates/aft/src/symbol_diff.rs` -- deterministic AST-backed symbol diffing between file revisions; `crates/aft/src/callgraph.rs` -- build navigation indexes; `crates/aft/src/calls.rs` -- extract call sites and Rust value references for reachability analysis; `crates/aft/src/backup.rs` -- manage sessionized backup stores, policies, and stack-level disk locks; `crates/aft/src/edit.rs` -- run shared edit and diff logic; `crates/aft/src/semantic_index.rs` -- dense-embedding semantic search index with query budget enforcement; `crates/aft/src/synapse_embed.rs` -- Synapse semantic embedding client over SubC daemon; `crates/aft/src/walk_boundary.rs` -- filesystem mount boundary guard for recursive directory walks; `crates/aft/src/search_index.rs` -- trigram-based full-text search index (with transient scratch cache directory sweeps); `crates/aft/src/grep_executor.rs` -- execute accelerated grep and glob searches with fallback query limits and budgets; `crates/aft/src/memory.rs` -- attribute process memory usage (reporting kernel physical footprint `phys_footprint_bytes` on macOS) and relieve allocator pressure; `crates/aft/src/logging.rs` -- write PID-scoped log files, enforce log rotation (32MB cap), reap dead process logs (24h quiet), and enforce directory storage budget (200MB); `crates/aft/src/gh_shim.rs` -- credential-free `gh` command routing and manifest verification; `crates/aft/src/github_read/mod.rs` -- parse, fetch, compress bot comments, normalize, render, and cache structured GitHub issue and pull request reads; `crates/aft/src/build_breaker.rs` -- durable circuit breaking and build suspension tracking across background domains; `packages/aft-bridge/src/revivable-transport.ts` -- wrap transport pools to revive connections when new requests arrive post-shutdown without reusing dead routes; `packages/aft-bridge/src/bash-host-fallback.ts` -- break-glass foreground bash fallback execution; `packages/aft-bridge/src/npm-resolver.ts` -- resolve npm executable, augment spawn PATH for GUI launches, invoke Windows cmd shims via cmd.exe, and terminate process trees; `crates/aft/src/compress/mod.rs` -- bash output compression dispatcher; `crates/aft/src/bash_background/` -- background task and PTY management with session-owned bash artifact read permissions, watch tombstones, and durable watch redelivery; `crates/aft/src/imports/` -- language-aware import engines; `crates/aft/src/inspect/` -- codebase health scanners and AST-based liveness graph; `crates/aft/src/format.rs` -- formatter detection and execution; `crates/aft/src/run_tool_call.rs` -- execute tool calls with translation and formatting; `crates/aft/src/runtime_drain.rs` -- drain watcher, search-index, callgraph-store, semantic-index, and LSP events on the request thread; `crates/aft/src/subc_translate.rs` -- translate tool arguments to internal command parameters (including decoding RFC 8089 `file:` URLs); `crates/aft/src/subc_format.rs` -- format/render agent-facing text on the server; `crates/aft/src/pty_render.rs` -- render raw PTY bytes with vt100 parsing; `crates/aft/src/response_finalize.rs` -- finalize protocol responses with completions and alert blocks; `crates/aft/src/alert_render.rs` -- format server-rendered `<system-reminder>` alerts; `crates/aft/src/alert_records.rs` -- record durable observation and disappearance rows in SQLite; `crates/aft/src/artifact_owner.rs` -- manage cache directory lease ownership; `crates/aft/src/readonly_artifacts.rs` -- strict read-only access to cached index files; `crates/aft/src/root_cache.rs` -- manage root-keyed writer leases, active reader coordination, and serialize artifact publication epochs; `crates/aft/src/cache_freshness.rs` -- track file metadata and utilize verification tickets to prevent race conditions during concurrent file invalidation; `crates/aft/src/fs_lock.rs` -- implement filesystem-based locking and writer lease verification; `crates/aft/src/legacy_partitions.rs` -- guard and inspect legacy layout harness caches; `crates/aft/src/cold_build_limiter.rs` -- serialize global cold-build slots with conditional admission; `crates/aft/src/sandbox_spawn.rs` -- resolve containment policies and native/host sandbox spawn plans; `crates/aft/src/sandbox_profile.rs` -- build platform-specific sandbox confinement profiles; `crates/aft/src/cli/sandbox_launch.rs` -- platform-specific execution wrappers for sandbox enforcement; `packages/aft-bridge/src/bridge.ts` -- manage subprocess transport; `packages/aft-bridge/src/pool.ts` -- session-scoped bridge pool; `packages/aft-bridge/src/subc-transport.ts` -- manage subconscious daemon transport; `packages/aft-bridge/src/transport-factory.ts` -- factory for transport pool instantiation; `packages/aft-bridge/src/transport.ts` -- define shared transport interfaces; `crates/aft/src/agent_child_env.rs` -- inject governed agent child execution environment, shims, and managed POSIX git hook dispatchers with quarantine safeguards; `crates/aft/src/scoped_key.rs` -- derive canonical `scoped-v1` artifact identities for standing roots; `crates/aft/src/db/standing_roots.rs` -- manage machine-scoped durable resolution for standing roots; `crates/aft/src/db/github_read_cache.rs` -- persist fallback cached GitHub issue and pull request documents.

**Tests:** `packages/opencode-plugin/src/__tests__/` -- plugin unit and e2e tests; `packages/pi-plugin/src/__tests__/` -- Pi plugin unit and e2e tests; `packages/aft-cli/src/__tests__/` -- CLI command tests; `packages/aft-bridge/src/__tests__/` -- bridge transport tests; `crates/aft/tests/integration/` -- Rust integration tests; `crates/aft/tests/semantic_test.rs` -- semantic index tests; `tests/docker/` -- Docker e2e; `tests/macos-e2e/` -- macOS e2e; `tests/windows-e2e/` -- Windows e2e; `tests/pi-rpc/` -- Pi RPC tests.

## Naming Conventions

**Files:** Use capability-oriented filenames. Put Rust command handlers in snake_case files such as `crates/aft/src/commands/move_symbol.rs`. Put TypeScript tool groups in concise nouns such as `packages/opencode-plugin/src/tools/navigation.ts`. Use `.test.ts` for plugin tests and `_test.rs` for Rust tests.

**Directories:** Use lower-case descriptive directories. Group related runtime code under `packages/opencode-plugin/src/tools/`, `packages/pi-plugin/src/tools/`, `crates/aft/src/commands/`, `crates/aft/src/lsp/`, `crates/aft/src/compress/`, `crates/aft/src/imports/`, `crates/aft/src/inspect/`, and `crates/aft/src/hashline/`.

## Where to Add New Code

**New hoisted OpenCode file tool:** `packages/opencode-plugin/src/tools/hoisted.ts` -- register the tool and map it onto the unified `tool_call` command.

**New tool argument translation/mapping:** `crates/aft/src/subc_translate.rs` -- define how client-facing tool arguments are translated to internal command parameters.

**New tool server-side text formatter:** `crates/aft/src/subc_format.rs` -- define how tool outputs are formatted/rendered to the agent.

**New plugin tool group (OpenCode):** `packages/opencode-plugin/src/tools/[capability].ts` -- export a `Record<string, ToolDefinition>` and wire it into `packages/opencode-plugin/src/index.ts`.

**New plugin tool group (Pi):** `packages/pi-plugin/src/tools/[capability].ts` -- export Pi tool definitions and wire them into `packages/pi-plugin/src/index.ts`.

**New shared bridge export:** `packages/aft-bridge/src/[module].ts` -- add shared transport, resolution, or formatting logic, then export from `packages/aft-bridge/src/index.ts`.

**New CLI command:** `packages/aft-cli/src/commands/[command].ts` -- add command handler and wire it into `packages/aft-cli/src/index.ts`.

**New Rust command handler:** `crates/aft/src/commands/[command_name].rs` -- expose the handler from `crates/aft/src/commands/mod.rs` and dispatch it from `crates/aft/src/main.rs`.

**New patch parser/matching code:** `crates/aft/src/patch/[module].rs` -- implement parsing or sequence matching logic and expose it via `crates/aft/src/patch/mod.rs`.

**New hashline apply repair or register rule:** `crates/aft/src/hashline/apply/` -- implement apply operations, repair logic, or register storage.

**New hashline oracle test fixture or vector:** `crates/aft/src/hashline/oracle/` -- add fixtures to `fixtures.jsonl` or test vectors to `xxhash32_vectors.rs`.

**New shared Rust engine code:** `crates/aft/src/[domain].rs` -- keep reusable parser, formatter, import, search, or analysis logic outside command handlers.

**New import language engine:** `crates/aft/src/imports/[language].rs` -- implement the `ImportSyntax` trait and register it in `crates/aft/src/imports/mod.rs`.

**New compression module:** `crates/aft/src/compress/[tool].rs` -- implement the `Compressor` trait and register it in `crates/aft/src/compress/mod.rs`.

**New inspection scanner:** `crates/aft/src/inspect/scanners/[scan].rs` -- add the scanner and register it in `crates/aft/src/inspect/scanners/mod.rs`.

**New LSP behavior:** `crates/aft/src/lsp/[module].rs` -- keep transport and server-management code inside the LSP subsystem.

**New sandbox confinement or backend rules:** `crates/aft/src/sandbox_profile.rs` (confinement profile definition), `crates/aft/src/sandbox_spawn.rs` (spawn policy logic), or `crates/aft/src/cli/sandbox_launch/` (OS-specific sandboxing backends).

**New bash rewrite rule:** `crates/aft/src/bash_rewrite/rules.rs` -- implement the `RewriteRule` trait, register the decision class in `catalog.rs`, and dispatch it in `dispatch.rs`.

**New alert observation or reminder renderer:** `crates/aft/src/alert_render.rs` (server-rendered reminder blocks), `crates/aft/src/alert_state.rs` (diagnostic identity tracking), or `crates/aft/src/alert_records.rs` (SQLite observation/disappearance records).

**New symbol diff comparison:** `crates/aft/src/symbol_diff.rs` -- implement symbol diff comparisons or language support rules.

**New management operation:** `crates/aft/src/commands/[operation].rs` -- implement passive or management operations (such as `health.digest` in `crates/aft/src/commands/health_digest.rs`) without agent-facing tool registration.

**New gh shim routing or manifest logic:** `crates/aft/src/gh_shim.rs` -- implement command routing rules, manifest validation, or offline self-report formats.

**New agent child environment or git hook/shim logic:** `crates/aft/src/agent_child_env.rs` -- configure agent child environment injection, shim binaries, managed git hook dispatchers, or commit attribution hooks.

**New standing root key or resolution logic:** `crates/aft/src/scoped_key.rs` (standing root key derivation) and `crates/aft/src/db/standing_roots.rs` (SQLite standing root persistence).

**New semantic embedding backend:** `crates/aft/src/synapse_embed.rs` -- implement `SynapseEmbeddingClient` or additional embedding backends and register in `crates/aft/src/config.rs` and `crates/aft/src/semantic_index.rs`.

**New filesystem boundary or mount guard:** `crates/aft/src/walk_boundary.rs` -- implement device boundary checks for recursive directory traversal.

**New GitHub read normalization or rendering rule:** `crates/aft/src/github_read/bot_compress.rs`, `crates/aft/src/github_read/normalize.rs`, or `crates/aft/src/github_read/render.rs` -- implement GitHub resource parsing, bot comment compression, markdown normalization, or canonical document rendering.

**New build breaker domain or suspension rule:** `crates/aft/src/build_breaker.rs` -- implement durable build circuit breaker domains, admission checks, or suspension liftoff rules.

**New platform binary package:** `packages/npm/[platform-key]/` -- add `package.json` and ship the platform binary in `bin/`.

**New plugin tests:** `packages/opencode-plugin/src/__tests__/` or `packages/pi-plugin/src/__tests__/` -- follow the existing `*.test.ts` naming.

**New Rust integration tests:** `crates/aft/tests/integration/` -- follow the existing `*_test.rs` naming.

**New benchmark:** `benchmarks/[name]/` -- create a benchmark directory with `src/`, `corpora/`, `results/`, and `scripts/` subdirectories.
