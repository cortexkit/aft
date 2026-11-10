# Architecture

A short map of how AFT is put together. Directory layout and "where does new
code go" live in [STRUCTURE.md](STRUCTURE.md); the agent-facing tool contract
lives in [docs/tools.md](docs/tools.md).

## What AFT is

AFT is a Rust engine (`crates/aft`, binary `aft`) that gives coding agents
structure-aware reading, editing, search, navigation, inspection, and bash.
Agents reach it in three ways:

- **Harness plugins.** `packages/opencode-plugin` (the OpenCode V1 plugin plus
  the OpenCode V2 runtime in `src/entry/server-runtime.mjs`) and
  `packages/pi-plugin` (Pi and the Pi-compatible OMP host). Both share
  `packages/aft-bridge`, which resolves or downloads the binary and owns the
  transport.
- **Session runners.** A runner that hosts sessions without an AFT plugin
  (Broca for delegated workers; CortexKit's harness modules for OpenCode 2,
  Claude Code and Pi sessions) fetches AFT's tool catalog and calls tools over
  the tool-provider role. AFT names these sessions harness `runner`
  (`opencode2` and `claude-code` parse to it).
- **MCP facades.** Clients outside the fleet bind as `mcp:<client>` and are
  untrusted (see Config and trust).

`packages/aft-cli` is the `npx @cortexkit/aft` CLI (setup, doctor, LSP and
filter management).

The engine runs in one of two modes. **Standalone**: the bridge spawns `aft`
per project root and speaks NDJSON over stdin/stdout. **Subc** (the normal
case): `aft --subc <connection-file>` connects to the Subconscious daemon over
loopback TCP, authenticates, and serves tool calls on per-session routes; the
bridge then talks to the daemon instead of a child process. The plugins use
subc when the user-tier `subc.connection_file` is set; a configured but
missing connection file fails loudly rather than falling back to standalone.

## Subsystems

**Tool-call pipeline.** Every tool call, from a plugin or a runner, becomes one
`tool_call` request (standalone) or one route call (subc). `run_tool_call.rs`
drives the same sequence for both: `subc_translate.rs` maps agent arguments
(including path aliases) onto a native command, the handler in `commands/`
runs, `subc_format.rs` renders the agent-visible text on the server, and
`response_finalize.rs` appends trailing lines such as alert reminders and the
status bar. Because the text is rendered on the server, every host shows the
same output. List-shaped results carry a truncation envelope
(`list_envelope.rs`, `list_surfaces/`) whose trailer reads
`shown N of M <unit> (<reason>) · narrow: <knobs>`.

**Tool-provider role.** `subc/tool_provider.rs` serves the CortexKit
tool-provider contract to runners: `role.describe`, `tool.catalog`,
`tool.views`, `tool.call`, `tool.withdraw` and `late_results`. A catalog is
chosen by preset (`head`, `worker` with `bash_watch`, `reader` with read and
search tools only) and, when the runner names the model, by model: GPT models
get `apply_patch`, others `edit` and `write`. A runner fetches a worker's
catalog once, so the preset's parameters (remote execution, sibling checkouts)
are frozen at that fetch and kept in `aft.db`, keyed on the session's bind
identity and scope (`subc/remote_policy.rs`); later calls only look them up.
Keyed calls are admitted and settled in the call ledger (`db/call_ledger.rs`),
so a reply lost to a restart is still delivered as a late result. Long calls
send invisible keepalives (`subc/keepalive.rs`). A finished background command
is pushed only to the caller that launched it (`subc/push.rs`).

**Scopes and runs.** The daemon stamps each route with the scope it serves: a
head, a worker, a flow, or a code-mode run. The stamp decides the catalog
preset, which saved grants apply and which processes belong to whom. When a
scope ends, AFT asks the daemon (`scope.describe`, `subc/scope_describe.rs`)
and kills that scope's shell processes only on a definite answer; a route
closing or a module restart alone kills nothing. Code mode (`subc/codemode.rs`,
`subc/run_scope.rs`) serves programs that BASAL runs: an agent run acts with
its head agent's access, a worker run with only its worker's. Background
commands a program starts belong to the agent it ran for. Flows get read-only
operations on call-scoped routes, never general tools (`subc/flow_read.rs`).

**Access policy.** Every path a tool touches goes through
`AppContext::validate_path` (writes) or `validate_read_path` (reads) in
`access_policy.rs`. `subc/call_policy.rs` installs each call's environment
first: the route's ask route, the scope whose saved grants apply, forced
project-root restriction for an untrusted bind, and a confined worker's root
fence. When the policy says ask for a worker's call, the call is held and the
question goes to Cingulate through the daemon (`subc/consent.rs`), never to a
host prompt; answers saved as grants live in `aft.db`
(`subc/access_grants.rs`). Plugin heads ask through their host's permission
prompt instead.

**Executor.** `executor/` schedules work per project root (an "actor"). Jobs
run on lanes: `PureRead`, `SerialLspStatus`, `HeavyInit`, `Mutating` (a writer
barrier reserved for configure and user mutations; a route bind starts with
only a shared hold beside readers and maintenance, and takes the barrier only
if it must change the root), and `MaintenanceCommit` (background drains that
must never block reads). Jobs are classed `Interactive` or `Maintenance` so
maintenance cannot starve tool calls; the maintenance queue is bounded and
idempotent drains coalesce. Long work checks a `JobCancellation` token at phase
boundaries. Process-wide heavy builds share slots through
`cold_build_limiter.rs`, and `build_breaker.rs` suspends a build domain that
keeps crashing.

**Index store.** Text search, the call graph and meaning search live in one
index store per repository (`indexes.trigram`, `indexes.callgraph`,
`indexes.semantic`; each on by default). `crates/aft-store` owns the formats:
immutable segment files of checksummed sections, read with positioned reads
through one process-wide block cache, with at most 1,024 segment files open at
once. A segment can hold text units with trigram postings, content records
(each file's parsed symbols, references and outline, keyed by content,
language and extractor version), call-graph fragments, and vectors (int8 rows
scanned, f32 rows for rescoring). Worktrees and clones of one repository share
a store under `storage/data/<family>`, keyed by the repository's root commits;
each checkout keeps its own state file naming its paths and their units. A
manifest is the commit record of each publication, readers lease the
generation they read, and cleanup never removes a file a live lease or the
newest manifest names.

`data_layer/` is AFT's side. A checkout binds to its published state and
rescans against disk before serving. A new worktree starts from the sibling
checkout whose state differs least and processes only the files that differ;
farther than a quarter of its paths, it builds cold, still reusing stored
records. Builds, refreshes and meaning-search fill run in small process-wide
pools (`data_layer/runtime.rs`); under the daemon the root's executor admits
each job, so an unbound root starts nothing. A first build publishes text
before the call graph, so `grep` is ready in about a second. Edits update text
at once; the call graph refreshes after two quiet minutes, or at once when a
call-graph reader asks (it waits up to 30 s, then reports building). The
incremental engine (`data_layer/incremental.rs`) re-resolves only files whose
recorded inputs changed, and changes are published as small segments once
enough accumulate. The resolver itself (`callgraph_store/`: extraction,
binding, dispatch) is shared with the gates that compare incremental results
with cold builds. `aft_search` blends lexical and meaning lanes
(`commands/semantic_search/`, `search_b2/`); embeddings come from fastembed
(local ONNX), an OpenAI-compatible endpoint, Ollama, or Synapse over subc
(`synapse_embed.rs`, chunking in `semantic_index.rs`). A plane that is still
building says so: `grep` and `glob` walk the folder until text is ready, and
call-graph tools answer "building" rather than from an old graph.
Parent-folder sessions (`data_layer/parent_session/`) answer from each child
repository's checkout. User-configured standing roots (`index.roots`,
`standing_roots.rs`, `scoped_key.rs`) are indexed by the subc standing actor
independent of sessions.

**Edit, patch, and hashline.** `edit.rs` holds the shared snapshot, mutate,
diff, and validate path used by `write`, `edit`, symbol edits, and refactors;
`format.rs` runs formatters and type checkers when configured. `patch/` parses
and applies `apply_patch` hunks (add, delete, update, move) with fuzzy matching
and all-or-nothing writes. With `edit_mode: "hashline"`, `read` renders tagged
lines and `edit` accepts a single patch addressed by those tags; `hashline/`
scans, verifies, composes, and applies it as a two-phase transaction
(`commands/hashline.rs`). Mutations record undo snapshots before writing (see
Backups).

**Bash.** Foreground, background, and PTY commands share one spawn path.
`bash_rewrite/` turns simple reads and searches (`cat`, `grep`, `ls`, ...)
into the equivalent AFT tool before anything runs; once a rewrite is accepted,
native bash is never run as a fallback. `bash_permissions/` scans the parsed
command for external paths and permission rules. `sandbox_spawn.rs` decides
the spawn plan: `sandbox.profile` sets a mode per role (`off`, `relaxed`,
`strict`; workers default to relaxed, heads to off), commands run under
Seatbelt on macOS or Landlock on Linux (`cli/sandbox_launch/`), and a mode that
cannot be enforced fails closed with `sandbox_unavailable`.
`agent_child_env.rs` builds the child environment: it strips daemon
credentials and puts the `gh` shim and managed git hooks on the child's path.
A foreground command that outlives its wait window becomes a background task in
`bash_background/` (task registry, PTY runtime, watchdog, pattern watches,
persistence across restarts). Output from a finished command is compressed by
`compress/`: tool-specific Rust compressors, output-shape sniffers,
package-manager compressors, TOML filters (builtin, user, and project; project
filters load only for trusted projects), then a generic fallback. Output from a
task that is still running is shown raw. `bash_db_hints/` adds a schema hint
when a database CLI fails on an unknown table or column.

**Remote execution.** With `remote_exec.enabled` (user tier), a bash call with
`runon` runs on a build server through the daemon's `exec-remote/v1`
capability (`exec_remote/`, `bash_background/remote.rs`). The job
gets a copy of the checkout; only output, the exit code and the files named in
`return_paths` come back, and AFT refuses a command that would edit tracked
files before submitting it. Commands may name secret placeholders; the vault
seam (`secret/`, `secret_proxy/`, `subc/secret_consent.rs`) resolves them per
call, but the production vault is not wired yet, so none resolve today.

**Inspect.** `aft_inspect` (`commands/inspect.rs`, `inspect/`) reports
diagnostics, metrics, and TODOs (tier 1) and cross-file categories such as dead
code, unused exports, duplicates, and cycles (tier 2). JS/TS liveness uses the
Oxc engine in `inspect/oxc_engine/`. Tier-2 results are cached and refreshed in
the background (`inspect/tier2_scheduler.rs`); an inspect call blocks until the
analysis is current and returns one terminal result (`FRESH`, `PARTIAL` with
named gaps, `INTERRUPTED`, or `PHASE-FAILED`), tracked by `inspect/phase_log.rs`.
`scope` narrows what is reported and which files get diagnostics; cross-file
categories are still computed over the whole project.

**LSP.** `lsp/` manages language-server processes per workspace root:
discovery and registry (`registry.rs`, `roots.rs`), JSON-RPC transport,
document sync, and a diagnostics store. Servers start lazily when a request
needs them and shut down after the idle TTL; `child_registry.rs` tracks and
reaps their processes. Plugins can auto-install missing servers. LSP-backed
features include diagnostics for inspect and edits, and navigation.

**Config and trust.** Config is JSONC at two tiers: user
`~/.config/cortexkit/aft.jsonc` and project `<root>/.cortexkit/aft.jsonc`,
each with optional `harnesses.<name>` overrides. In subc mode AFT reads both
files itself and ignores config sent over the wire. `config_resolve.rs` parses
both strictly and merges them, and it drops project values for
security-relevant keys: path restriction, semantic backend endpoints and
credentials, LSP executables, subc transport, remote execution, GitHub
capabilities, and anything that weakens the sandbox. For these keys a project
can only tighten: raise a sandbox mode or add read denies, switch an index or
remote execution off, or disable tools other than `aft_safety` and the host
tool slots. Retired
key names are translated with a notice, and AFT rewrites the user file to
current keys when it loads it. Separately, the subc edge assigns each route
bind a `BindTrust` (`subc/mod.rs`): first-party principals get full replies; a
reserved module is first-party only for the exact (module, harness) pairs it
has been checked for; untrusted binds (MCP facades, unverified principals) get
text-only replies, forced project-root path restriction, no bash-state
observation, and bash only after a host permission grant. See
[docs/config.md](docs/config.md).

**`gh` shim and GitHub reads.** The same binary acts as a `gh` shim when
invoked as `gh` or `aft gh-shim` (`gh_shim.rs`). Governed children find it
first on their `PATH`. It routes governed commands through the daemon under a
signed routing manifest, passes other commands to the real `gh`, and refuses
with exit 86 when governance cannot be established. Bot writes from an agent's
child go through `gh_shim_relay/`: the shim holds no daemon credentials, so it
sends a per-command ticket and the daemon makes the call as AFT;
`gh_shim_speech/` binds that write authority to the launching session. With
`github.read` enabled, `read`, `aft_outline`, and `aft_zoom` accept `issue://N`
and `pr://N`; `github_read/` fetches live through `gh`, compresses bot comments,
renders numbered discussion ordinals, and keeps the last good render in SQLite
as a disclosed fallback. See [docs/gh-shim.md](docs/gh-shim.md).

**Apps: display records and page bundles.** For the Alfonso apps,
`subc/rich_ui/` keeps display records of tool calls and checks every app read
against the caller's session and scope; `subc/tool_views.rs` serves the view
templates (`tool.views`) keyed by the exact tool definition each call was made
with. `page_collect/` reads an HTML page an agent wrote, and every file it
names, under that agent's read policy into a private snapshot for a sealed
page bundle.

**Backups and checkpoints.** `backup.rs` records an undo snapshot for every
mutation in a per-session, per-path store under the storage root (bounded
depth and file size, configured under `backup`); `aft_safety undo` pops it.
`checkpoint.rs` keeps named, session-scoped checkpoints on disk with a
per-session count limit and time-based retention. `agent_data/` carries a
session's undo and checkpoint data out through engram's session door.

**Health and alerts.** `alert_state.rs` ingests authoritative LSP diagnostic
snapshots, `alert_render.rs` tracks what is new per session and root, and the
response finalizer appends `<system-reminder>` alerts and the
`[AFT E.. W.. | ..]` status bar to tool results. `repeat_breaker.rs` adds a
reminder when an agent repeats the same call. `fleet_status.rs` publishes the
status segment to the fleet status holder when one is live. `health.digest`
(`commands/health_digest.rs`) is a management operation, not an agent tool,
that returns current counts with freshness tickets. The subc health report
(`subc/health.rs`) exposes readiness, memory (`memory.rs`), and anything that
keeps an idle root from being reclaimed.

**Lifecycle and storage.** `commands/configure.rs` binds a root: it resolves
config, starts the platform watcher (`watcher_backend/`: FSEvents, inotify, or
Windows), and schedules index warmup after acknowledging the bind. The home
folder and filesystem roots are never indexed. When a root loses its last route
it is quiesced: queued maintenance is cancelled, index builders are retired,
and warm artifacts are kept for a quick rebind. After `idle.root_ttl_minutes`
the artifacts are evicted. SQLite state (backups, bash tasks, watches, call
ledger, access grants, standing roots, GitHub cache, breaker rows) lives in
`aft.db` (`db/`); storage defaults to `~/.local/share/cortexkit/aft/`
(override with `AFT_STORAGE_DIR`).

## Code map

| Subsystem | Where |
|---|---|
| Harness plugins | `packages/opencode-plugin/`, `packages/pi-plugin/` |
| Bridge, transports, binary resolution | `packages/aft-bridge/src/` (`transport-factory.ts`, `pool.ts`, `bridge.ts`, `subc-transport.ts`, `resolver.ts`) |
| CLI | `packages/aft-cli/src/`; native subcommands in `crates/aft/src/cli/` |
| Process entry, standalone loop | `crates/aft/src/main.rs`, `protocol.rs` |
| Subc edge (binds, trust, push, drain) | `crates/aft/src/subc/` |
| Tool-call pipeline | `run_tool_call.rs`, `subc_translate.rs`, `subc_format.rs`, `response_finalize.rs`, `commands/tool_call.rs` |
| Tool-provider role | `subc/tool_provider.rs`, `subc/remote_policy.rs`, `subc/keepalive.rs`, `db/call_ledger.rs`, `db/catalog_edit_modes.rs` |
| Scopes, runs, code mode | `subc/scope_describe.rs`, `subc/run_scope.rs`, `subc/codemode.rs`, `subc/flow_read.rs` |
| Access policy, consent, grants | `access_policy.rs`, `subc/call_policy.rs`, `subc/consent.rs`, `subc/access_grants.rs`, `db/access_grants.rs` |
| Command handlers | `crates/aft/src/commands/` |
| Executor | `crates/aft/src/executor/`, `cold_build_limiter.rs`, `build_breaker.rs` |
| Runtime state | `context.rs` (`AppContext`) |
| Index store formats | `crates/aft-store/` |
| Index store in AFT | `data_layer/` (checkout, runtime, incremental, meaning, `tools/` adapters) |
| Text search | `grep_executor.rs`, `data_layer/tools/grep.rs` |
| Semantic search | `commands/semantic_search/`, `search_b2/`, `synapse_embed.rs`, `semantic_index.rs`, `data_layer/vectors.rs` |
| Call graph | `callgraph_store/` (resolver), `data_layer/callgraph.rs`, `callgraph.rs`, `calls.rs` |
| Parent-folder sessions | `data_layer/parent_session/`, `data_layer/parent.rs` |
| Standing roots | `standing_roots.rs`, `scoped_key.rs`, `subc/standing.rs`, `db/standing_roots.rs` |
| Parsing and symbols | `parser.rs`, `symbols.rs`, `symbol_cache_disk.rs`, `language.rs`, `imports/` |
| Edit, patch, hashline | `edit.rs`, `format.rs`, `patch/`, `hashline/`, `fuzzy_match.rs` |
| Bash | `bash_rewrite/`, `bash_permissions/`, `bash_background/`, `bash_db_hints/`, `sandbox_spawn.rs`, `sandbox_profile.rs`, `agent_child_env.rs`, `pty_render.rs` |
| Remote execution, secrets | `exec_remote/`, `bash_background/remote.rs`, `secret/`, `secret_proxy/`, `subc/secret_consent.rs` |
| Output compression | `compress/` |
| Inspect | `inspect/`, `commands/inspect.rs` |
| LSP | `lsp/` |
| Config | `config.rs`, `config_resolve.rs`, `feature_config.rs`, `config_fix.rs`, `subc_config.rs` |
| GitHub | `gh_shim.rs`, `gh_shim_relay/`, `gh_shim_speech/`, `github_read/`, `db/github_read_cache.rs` |
| Apps: display records, page bundles | `subc/rich_ui/`, `subc/tool_views.rs`, `page_collect/`, `db/rich_ui.rs` |
| Backups, checkpoints | `backup.rs`, `checkpoint.rs`, `agent_data/`, `db/backups.rs` |
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
- **Kill only on a confirmed scope end.** A closed route, a drain or a module
  restart is not a scope end; only the daemon's `scope.describe` answer is.
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
- [docs/design/subc-readiness-warmup.md](docs/design/subc-readiness-warmup.md): subc readiness and warmup
- [docs/design/opencode-v2-plugin-playbook.md](docs/design/opencode-v2-plugin-playbook.md): OpenCode V2 plugin
- [docs/investigations/watcher-backends-reference.md](docs/investigations/watcher-backends-reference.md): watcher backends
- [docs/ops/aft-health-sentinel.md](docs/ops/aft-health-sentinel.md): health monitoring
- [docs/benchmarks.md](docs/benchmarks.md): benchmarks
