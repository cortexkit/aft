# Architecture

A short map of how AFT is put together. Directory layout and "where does new
code go" live in [STRUCTURE.md](STRUCTURE.md); the agent-facing tool contract
lives in [docs/tools.md](docs/tools.md).

## What AFT is

AFT is a Rust engine (`crates/aft`, binary `aft`) that gives coding agents
structure-aware reading, editing, search, navigation, inspection, and bash.
Agents reach it through TypeScript harness plugins: `packages/opencode-plugin`
(OpenCode V1 plugin entry plus the OpenCode V2 Effect runtime in
`src/entry/server-runtime.mjs`) and `packages/pi-plugin` (Pi and the
Pi-compatible OMP host). Both plugins share `packages/aft-bridge`, which
resolves or downloads the binary and owns the transport. `packages/aft-cli`
is the `npx @cortexkit/aft` CLI (setup, doctor, LSP and filter management).
The engine runs in one of two modes. **Standalone** (the default): the bridge
spawns `aft` per project root and speaks NDJSON over stdin/stdout.
**Subc**: when the user-tier `subc.connection_file` is set, `aft --subc
<connection-file>` connects to the Subconscious daemon over loopback TCP,
authenticates, and serves tool calls on per-session routes; the bridge talks to
the daemon instead of a child process. A configured but missing connection
file fails loudly rather than silently falling back to standalone.

## Subsystems

**Tool-call pipeline.** Plugins register thin tool adapters that forward the
agent's tool name and arguments as one `tool_call` request (standalone) or one
route call (subc). In Rust, `run_tool_call.rs` drives the same sequence for
both transports: `subc_translate.rs` maps agent arguments (including path
aliases) onto a native command and its parameters, the handler in
`commands/` runs, `subc_format.rs` renders the agent-visible text on the
server, and `response_finalize.rs` appends trailing lines such as alert
reminders and the status bar. Because the agent-visible text is rendered on
the server, standalone and subc sessions show the same output.
List-shaped results carry a truncation envelope (`list_envelope.rs`,
`list_surfaces/`) whose text trailer reads
`shown N of M <unit> (<reason>) · narrow: <knobs>`.

**Executor.** `executor/` schedules work per project root (an "actor").
Jobs run on lanes: `PureRead`, `SerialLspStatus`, `HeavyInit`, `Mutating`
(a writer barrier reserved for configure and user mutations), and
`MaintenanceCommit` (background drains that must never block reads). Jobs are
classed `Interactive` or `Maintenance` so maintenance cannot starve tool calls;
the maintenance queue is bounded and idempotent drains coalesce. Long work
checks a `JobCancellation` token at phase boundaries. Process-wide heavy builds
share slots through `cold_build_limiter.rs`, and `build_breaker.rs` suspends a
build domain that keeps crashing.

**Indexes.** Three background indexes, each on by default (`indexes.*` in
config):
- *Trigram* (`search_index.rs`): a disk-backed postings file read with
  positioned reads, serving indexed `grep`/`glob` and the lexical lane of
  `aft_search`. `grep_executor.rs` falls back to a bounded walk while the index
  is building or unavailable.
- *Semantic* (`semantic_index.rs`): dense embeddings from fastembed (local
  ONNX), an OpenAI-compatible endpoint, Ollama, or Synapse over subc
  (`synapse_embed.rs`). `aft_search` blends lexical and semantic lanes
  (`commands/semantic_search/`, `search_b2/`) and degrades to lexical when a
  query embedding times out.
- *Callgraph store* (`callgraph_store/`): a persisted SQLite graph of call
  edges behind `aft_callgraph` and zoom enrichment. Each build writes a new
  generation file and swaps a `.current` pointer, so readers never see a
  half-built graph. `callgraph.rs` keeps the lighter per-file call data.

Artifacts live in root-keyed cache directories under the storage root. The
first checkout of a repository to claim `owner.json` (`artifact_owner.rs`)
becomes the writer; `root_cache.rs` adds per-domain writer leases, reader
markers, and publish epochs so a superseded worker cannot publish stale data.
Another live checkout of the same repository, typically a linked worktree,
runs **borrow-only**: it opens the owner's search and semantic indexes through
read-only openers (`readonly_artifacts.rs`) and the callgraph through
`ReadonlyCallGraphStore`, and never builds or repairs them. `worktree.ram_overlay`
lets a borrow-only checkout layer its own edits into private in-RAM search
deltas. Optional content-addressed views (`views/`, `blob_store/`, `pins/`,
`gc/`, `refresh/`; off by default) assemble semantic and callgraph artifacts
from per-file blobs behind an atomic manifest. User-configured standing roots
(`index.roots`, `standing_roots.rs`, `scoped_key.rs`) are indexed by the subc
standing actor independent of sessions.

**Edit, patch, and hashline.** `edit.rs` holds the shared snapshot, mutate,
diff, and validate path used by `write`, `edit`, symbol edits, and refactors;
`format.rs` runs formatters and type checkers when configured. `patch/`
parses and applies `apply_patch` hunks (add, delete, update, move) with fuzzy
matching and all-or-nothing writes. With `edit_mode: "hashline"`, `read`
renders tagged lines and `edit` accepts a single patch addressed by those tags;
`hashline/` scans, verifies, composes, and applies it as a two-phase
transaction (`commands/hashline.rs`). Mutations record undo snapshots before
writing (see Backups).

**Bash.** Foreground, background, and PTY commands share one spawn path.
`bash_rewrite/` turns simple reads and searches (`cat`, `grep`, `ls`, ...)
into the equivalent AFT tool before anything runs; once a rewrite is
accepted, native bash is never run as a fallback. `bash_permissions/` scans
the parsed command for external paths and permission rules. `sandbox_spawn.rs`
decides the spawn plan: with `sandbox.enabled`, first-party commands run under
Seatbelt on macOS or Landlock on Linux (`cli/sandbox_launch/`), and any
platform or policy that cannot be enforced fails closed with
`sandbox_unavailable`. `agent_child_env.rs` builds the child environment: it
strips daemon credentials and puts the `gh` shim and managed git hooks on the
child's path. A foreground command that outlives its wait window becomes a
background task in `bash_background/` (task registry, PTY runtime, watchdog,
pattern watches, persistence across restarts); completions reach the session
as pushed wake nudges. Output from a finished command is compressed by
`compress/`: tool-specific Rust compressors, output-shape sniffers,
package-manager compressors, TOML filters (builtin, user, and project; project
filters load only for trusted projects), then a generic fallback. Output from a
task that is still running is shown raw.

**Inspect.** `aft_inspect` (`commands/inspect.rs`, `inspect/`) reports
diagnostics, metrics, and TODOs (tier 1) and cross-file categories such as dead
code, unused exports, duplicates, and cycles (tier 2). JS/TS liveness uses
the Oxc engine in `inspect/oxc_engine/`. Tier-2 results are cached and
refreshed in the background (`inspect/tier2_scheduler.rs`); an inspect call
blocks until the analysis is current and returns exactly one terminal result
(`FRESH`, `INTERRUPTED`, or `PHASE-FAILED`), tracked by `inspect/phase_log.rs`.
`scope` narrows what is shown, not what is verified.

**LSP.** `lsp/` manages language-server processes per workspace root:
discovery and registry (`registry.rs`, `roots.rs`), JSON-RPC transport,
document sync, and a diagnostics store. Servers start lazily when a request
needs them and shut down after the idle TTL; `child_registry.rs` tracks and
reaps their processes. Plugins can auto-install missing servers. LSP-backed
features include diagnostics for inspect and edits, and navigation.

**Config and trust tiers.** Config is JSONC at two tiers: user
`~/.config/cortexkit/aft.jsonc` and project `<root>/.cortexkit/aft.jsonc`,
each with optional `harnesses.<name>` overrides. `config_resolve.rs` parses
both strictly and merges them, and it drops project values for security-relevant
keys: path restriction, semantic backend endpoints and credentials, LSP
executables, subc transport, GitHub capabilities, and anything that weakens
the sandbox. For these keys a project can only tighten: enable the sandbox,
switch an index off, or disable tools other than `aft_safety` and the host
tool slots. Separately, the subc edge assigns each route bind a
`BindTrust` (`subc/mod.rs`): first-party principals get full replies;
untrusted ones (MCP facades, unverified principals) get text-only replies,
forced project-root path restriction, no bash-state observation, and bash only
after a host permission grant. See [docs/config.md](docs/config.md).

**`gh` shim and GitHub reads.** The same binary acts as a `gh` shim when
invoked as `gh` or `aft gh-shim` (`gh_shim.rs`). Governed children find it
first on their `PATH`. It routes governed commands through the daemon under a
signed routing manifest, passes other commands to the real `gh`, and refuses
with exit 86 when governance cannot be established. With `github.read`
enabled, `read`, `aft_outline`, and `aft_zoom` accept `issue://N` and `pr://N`;
`github_read/` fetches live through `gh`, compresses bot comments, renders
numbered discussion ordinals, and keeps the last good render in SQLite as a
disclosed fallback. See [docs/gh-shim.md](docs/gh-shim.md).

**Backups and checkpoints.** `backup.rs` records an undo snapshot for every
mutation in a per-session, per-path store under the storage root (bounded
depth and file size, configured under `backup`); `aft_safety undo` pops it.
`checkpoint.rs` keeps named, session-scoped checkpoints on disk with a
per-session count limit and time-based retention.

**Health and alerts.** `alert_state.rs` ingests authoritative LSP
diagnostic snapshots, `alert_render.rs` tracks what is new per session and
root, and the response finalizer appends `<system-reminder>` alerts and the
`[AFT E.. W.. | ..]` status bar to tool results. `repeat_breaker.rs` adds a
reminder when an agent repeats the same call. `fleet_status.rs` publishes the
status segment to the fleet status holder when one is live. `health.digest`
(`commands/health_digest.rs`) is a management operation, not an agent tool,
that returns current counts with freshness tickets. The subc health report
(`subc/health.rs`) exposes readiness, memory (`memory.rs`), and anything that
keeps an idle root from being reclaimed.

**Lifecycle and storage.** `commands/configure.rs` binds a root: it resolves
config, starts the platform watcher (`watcher_backend/`: FSEvents, inotify, or
Windows), and schedules index warmup after acknowledging the bind. When a root
loses its last route it is quiesced: queued maintenance is cancelled, index
builders are retired, and warm artifacts are kept for a quick rebind. After
`idle.root_ttl_minutes` the artifacts are evicted. SQLite state (backups, bash
tasks, watches, standing roots, GitHub cache, breaker rows) lives in `db/`;
storage defaults to `~/.local/share/cortexkit/aft/` (override with
`AFT_STORAGE_DIR`).

## Code map

| Subsystem | Where |
|---|---|
| Harness plugins | `packages/opencode-plugin/`, `packages/pi-plugin/` |
| Bridge, transports, binary resolution | `packages/aft-bridge/src/` (`transport-factory.ts`, `pool.ts`, `bridge.ts`, `subc-transport.ts`, `resolver.ts`) |
| CLI | `packages/aft-cli/src/`; native subcommands in `crates/aft/src/cli/` |
| Process entry, standalone loop | `crates/aft/src/main.rs`, `protocol.rs` |
| Subc edge | `crates/aft/src/subc/` |
| Tool-call pipeline | `run_tool_call.rs`, `subc_translate.rs`, `subc_format.rs`, `response_finalize.rs`, `commands/tool_call.rs` |
| Command handlers | `crates/aft/src/commands/` |
| Executor | `crates/aft/src/executor/`, `cold_build_limiter.rs`, `build_breaker.rs` |
| Runtime state | `context.rs` (`AppContext`) |
| Trigram search | `search_index.rs`, `grep_executor.rs` |
| Semantic search | `semantic_index.rs`, `synapse_embed.rs`, `commands/semantic_search/`, `search_b2/` |
| Callgraph | `callgraph_store/`, `callgraph.rs`, `calls.rs` |
| Artifact ownership and borrowing | `artifact_owner.rs`, `root_cache.rs`, `readonly_artifacts.rs`, `legacy_partitions.rs` |
| Index views | `views/`, `blob_store/`, `pins/`, `gc/`, `refresh/`, `path_status/`, `alias/`, `migration/` |
| Standing roots | `standing_roots.rs`, `scoped_key.rs`, `subc/standing.rs`, `db/standing_roots.rs` |
| Parsing and symbols | `parser.rs`, `symbols.rs`, `symbol_cache_disk.rs`, `language.rs`, `imports/` |
| Edit, patch, hashline | `edit.rs`, `format.rs`, `patch/`, `hashline/`, `fuzzy_match.rs` |
| Bash | `bash_rewrite/`, `bash_permissions/`, `bash_background/`, `sandbox_spawn.rs`, `sandbox_profile.rs`, `agent_child_env.rs`, `pty_render.rs` |
| Output compression | `compress/` |
| Inspect | `inspect/`, `commands/inspect.rs` |
| LSP | `lsp/` |
| Config | `config.rs`, `config_resolve.rs`, `subc_config.rs` |
| GitHub | `gh_shim.rs`, `github_read/`, `db/github_read_cache.rs` |
| Backups, checkpoints | `backup.rs`, `checkpoint.rs`, `db/backups.rs` |
| Health, alerts | `alert_state.rs`, `alert_render.rs`, `alert_records.rs`, `fleet_status.rs`, `commands/health_digest.rs`, `subc/health.rs`, `memory.rs` |
| Watcher | `watcher/`, `watcher_backend/`, `watcher_filter.rs` |
| Persistence | `db/` |

Rust paths are relative to `crates/aft/src/` unless shown in full.

## Honest Reporting Convention

Every response lets the agent tell three states apart:

1. **Could not do the work**: `success: false` with a machine-readable `code`
   and a `message`.
2. **Did the work, complete**: `success: true` and, where results can be
   partial, `complete: true`, meaning an absent item really is absent.
3. **Did the work, partial**: `success: true`, `complete: false`, and a named
   gap (`pending_files`, `unchecked_files`, `skipped_files: [{file, reason}]`,
   `scope_warnings`, ...). A scope that matched nothing says
   `no_files_matched_scope: true` rather than returning an empty success.
   Skipped side steps use a specific `<step>_skipped_reason`.

The canonical field list is the `Response` doc comment in
`crates/aft/src/protocol.rs`.

## Rules that bite

- **Report honestly.** Never return an empty success for work that was not
  done; name the gap. Silent partial results are the bug this codebase guards
  against most.
- **Path restriction.** File access goes through `AppContext::validate_path`
  (mutations) or `validate_read_path` (reads, which may also open a session's
  own bash output files). Restriction is on with `restrict_to_project_root`
  and always forced for untrusted binds; symlinks are resolved so they cannot
  escape the root.
- **Never open a second file descriptor on a live SQLite file set.** Closing
  any descriptor to a file releases every POSIX advisory lock the process holds
  on it, so another process can reset the shared-memory file under a live
  mapping and crash the daemon. Go through the owning connection and SQLite's
  own APIs. Never read `-wal` or `-shm` directly, and never copy a database
  that is still open.
- **Apply bounds at the iterator.** A cap or deadline must stop the walk or
  query that produces items, not trim a fully collected list afterwards.
- **A root with no bound route does no indexing.** Unbound roots run no
  indexing or maintenance; late async work must not reactivate them.
  Configured standing roots are the only exception, and the standing actor
  owns them.
- **Every subc request gets a terminal frame.** The daemon holds a request's
  credit until the module sends `Response`, `Error`, or `StreamEnd` for it. Any
  path that stops tracking a request (cancel, root reclaim, drain) must send
  that frame itself (`subc/drain.rs`).
- **Test doubles are no more capable than the real thing.** A fake that
  succeeds where the real transport, filesystem, or server would fail proves
  nothing about production.

## Further reading

- [docs/tools.md](docs/tools.md): every agent tool and its parameters
- [docs/config.md](docs/config.md): config keys, trust tiers,
  [native sandbox](docs/config.md#native-command-sandbox),
  [GitHub integration](docs/config.md#github-integration), LSP
- [docs/cli.md](docs/cli.md): the `aft` and `npx @cortexkit/aft` CLIs
- [docs/hashline.md](docs/hashline.md): hashline patch grammar
- [docs/gh-shim.md](docs/gh-shim.md): `gh` shim routing and refusals
- [docs/health-digest.md](docs/health-digest.md): the `health.digest` operation
- [docs/memory-census.md](docs/memory-census.md): memory attribution
- [docs/design/content-addressed-index-views.md](docs/design/content-addressed-index-views.md): index views
- [docs/design/subc-readiness-warmup.md](docs/design/subc-readiness-warmup.md): subc readiness and warmup
- [docs/design/opencode-v2-plugin-playbook.md](docs/design/opencode-v2-plugin-playbook.md): OpenCode V2 plugin
- [docs/investigations/watcher-backends-reference.md](docs/investigations/watcher-backends-reference.md): watcher backends
- [docs/ops/aft-health-sentinel.md](docs/ops/aft-health-sentinel.md): health monitoring
- [docs/benchmarks.md](docs/benchmarks.md): benchmarks
