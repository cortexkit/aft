# Architecture

## Pattern Overview

**Overall:** TypeScript plugin + Rust worker process communicating over either a session-scoped NDJSON bridge (standalone mode) or the Subconscious (subc) daemon transport. A unified CLI (`packages/aft-cli/`) serves setup/doctor across all harnesses; shared transport, binary resolution, and ONNX helpers live in `packages/aft-bridge/`.

**Key Characteristics:**
- Use the master configuration switch `enabled` (configured globally or per-project in `aft.jsonc`) to short-circuit plugin loading and disable AFT execution.
- Use `packages/opencode-plugin/src/index.ts` and `packages/pi-plugin/src/index.ts` to register harness tools and map them onto the unified `tool_call` command when enabled.
- Use `packages/aft-bridge/src/transport-factory.ts` to instantiate either `BridgePool` (standalone NDJSON bridge, isolating one `aft` process per project root) or `SubcTransportPool` (daemon-backed transport) satisfying the shared `AftTransportPool` interface.
- Use `packages/aft-cli/src/index.ts` as the unified setup/doctor CLI across all harnesses.
- Use `crates/aft/src/commands/` handlers to keep protocol dispatch thin and command logic modular, with `crates/aft/src/commands/tool_call.rs` acting as the single endpoint for tool invocation routing.
- Use `crates/aft/src/edit.rs`, `crates/aft/src/format.rs`, `crates/aft/src/callgraph.rs`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/inspect/` (including codebase-health scanners and the `oxc_engine/` liveness solver), `crates/aft/src/hashline/` (including byte scanning, line-tag snapshots, syntax verifier, apply repair, same-path section composition, two-phase transactions, remap recovery, release gates, and seed-zero xxHash32 tag normalization oracle), `crates/aft/src/semantic_index.rs`, `crates/aft/src/synapse_embed.rs`, `crates/aft/src/search_index.rs`, `crates/aft/src/grep_executor.rs`, `crates/aft/src/memory.rs`, `crates/aft/src/logging.rs`, `crates/aft/src/fleet_status.rs`, `crates/aft/src/compress/`, `crates/aft/src/bash_rewrite/`, `crates/aft/src/patch/`, `crates/aft/src/pty_render.rs`, `crates/aft/src/response_finalize.rs`, `crates/aft/src/lsp/`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, `crates/aft/src/legacy_partitions.rs`, `crates/aft/src/cold_build_limiter.rs`, `crates/aft/src/symbol_diff.rs`, `crates/aft/src/alert_render.rs`, `crates/aft/src/alert_records.rs`, `crates/aft/src/alert_state.rs`, `crates/aft/src/gh_shim.rs`, `crates/aft/src/agent_child_env.rs`, `crates/aft/src/scoped_key.rs`, and `crates/aft/src/walk_boundary.rs` as shared engines behind multiple commands.

## Layers

**OpenCode integration layer:**
- Purpose: Register tools, load config, and attach post-execution metadata.
- Location: `packages/opencode-plugin/src/index.ts`
- Contains: Plugin bootstrap, tool-surface selection, tool registration map builder (`packages/opencode-plugin/src/tool-registration.ts`), hoisting logic, disabled-tool filtering, session-directory management, RPC server (exposing a live WebSocket endpoint for TUI notification and status invalidation pushes), auto-update checker hook
- Depends on: `packages/opencode-plugin/src/config.ts`, `packages/opencode-plugin/src/tools/*.ts`, `packages/aft-bridge/`
- Used by: OpenCode plugin loading through `@cortexkit/aft-opencode`

**Pi integration layer:**
- Purpose: Register tools, load config, and manage Pi host notifications.
- Location: `packages/pi-plugin/src/index.ts`
- Contains: Plugin bootstrap, tool-surface selection, tool registration helper (`packages/pi-plugin/src/tool-registration.ts`), hoisting logic, LSP auto-install (npm/github/project-relevance probes), `aft-status` command
- Depends on: `packages/pi-plugin/src/config.ts`, `packages/pi-plugin/src/tools/*.ts`, `packages/pi-plugin/src/commands/*.ts`, `packages/aft-bridge/`
- Used by: Pi coding agent through `@cortexkit/aft-pi`

**Shared bridge layer:**
- Purpose: Resolve or download the binary, start worker processes, manage ONNX runtime, format output, select and manage the transport pool, and forward requests. All harness adapters share this layer.
- Location: `packages/aft-bridge/src/bridge.ts`, `packages/aft-bridge/src/pool.ts`, `packages/aft-bridge/src/subc-transport.ts`, `packages/aft-bridge/src/revivable-transport.ts`, `packages/aft-bridge/src/transport.ts`, `packages/aft-bridge/src/transport-factory.ts`, `packages/aft-bridge/src/resolver.ts`, `packages/aft-bridge/src/downloader.ts`, `packages/aft-bridge/src/npm-resolver.ts`, `packages/aft-bridge/src/onnx-runtime.ts`, `packages/aft-bridge/src/migration.ts`, `packages/aft-bridge/src/zoom-format.ts`, `packages/aft-bridge/src/path-aliases.ts`, `packages/aft-bridge/src/error-contract.ts`, `packages/aft-bridge/src/cache-paths.ts`, `packages/aft-bridge/src/lifecycle-registry.ts`, `packages/aft-bridge/src/durable-log.ts`, `packages/aft-bridge/src/bash-host-fallback.ts`
- Contains: Transport factory routing selection (via user-tier `subc.connection_file`), subc client connection pooling, route caching per session-identity, background event subscriptions with independent reconnects, revivable transport pool wrapper (`packages/aft-bridge/src/revivable-transport.ts`) instantiating replacement pools on demand when new traffic arrives post-shutdown, break-glass bash host fallback execution (`packages/aft-bridge/src/bash-host-fallback.ts`) executing approved commands directly in the foreground without AFT processing when transport is down, session bridge lifecycle, restart handling, version checks, binary discovery, binary download, fallback npm resolution and spawn environment augmentation for PATH-stripped GUI launches (including Windows cmd.exe npm.cmd shims and process-tree termination in `packages/aft-bridge/src/npm-resolver.ts`), ONNX runtime detection, storage migration, compact UI formatting, active logger, durable log sinks (`packages/aft-bridge/src/durable-log.ts`), wait-aware transport budgets propagation (mapping `transportTimeoutMs` to route requests to avoid premature client-side timeouts during long command execution), canonical path alias resolution (`packages/aft-bridge/src/path-aliases.ts`), host-neutral error adaptation (`packages/aft-bridge/src/error-contract.ts`, mapping route GOODBYE and standalone post-write / sibling-timeout crashes to unknown-outcome disposition to prevent duplicate mutations), per-realm subc lifecycle management (`packages/aft-bridge/src/lifecycle-registry.ts`), and consolidated cache root resolution (`packages/aft-bridge/src/cache-paths.ts`)
- Depends on: Node child-process APIs, GitHub releases, `onnxruntime-node`, `@cortexkit/subc-client`
- Used by: `packages/opencode-plugin/src/index.ts`, `packages/pi-plugin/src/index.ts`

**Unified CLI layer:**
- Purpose: Provide a single `npx @cortexkit/aft` entry point for setup, doctor, and LSP management across all harnesses.
- Location: `packages/aft-cli/src/index.ts`, `packages/aft-cli/src/commands/`
- Contains: `setup`, `doctor`, `doctor lsp`, `doctor --fix`, `doctor --clear`, `doctor --issue`; harness auto-detection (OpenCode/Pi) with `--harness` override; inlines `packages/aft-bridge/` in CLI bundle via literal specifiers to prevent dynamic runtime module resolution errors under `npx`
- Depends on: `packages/aft-bridge/`, harness adapter config paths
- Used by: End users via `npx @cortexkit/aft`

**Tool definition layer (OpenCode):**
- Purpose: Convert OpenCode tool arguments into the unified `tool_call` protocol request and perform permission checks.
- Location: `packages/opencode-plugin/src/tools/`
- Contains: Hoisted tools (edit/write/apply_patch; where read, write, and edit advertise filePath to satisfy OpenCode's host UI header display contract while accepting path), reading tools, import tools, navigation tools, refactoring tools, safety tools, bash tools, conflict tools, AST tools, search tools, semantic tools (governed by isolated `aft_search` host permission checks independent from `grep`), inspect tools, permissions helpers, and the `callToolCall` transport wrapper (`packages/opencode-plugin/src/tools/_shared.ts`)
- Depends on: `packages/aft-bridge/src/pool.ts`, `packages/opencode-plugin/src/shared/`
- Used by: `packages/opencode-plugin/src/index.ts`

**Tool definition layer (Pi):**
- Purpose: Convert Pi tool arguments into the unified `tool_call` protocol request and perform permission checks.
- Location: `packages/pi-plugin/src/tools/`
- Contains: Hoisted tools (read/write/edit/grep) supporting cross-harness compatibility aliases (e.g. accepting `filePath` for `path` or vice versa), reading tools, import tools, structure tools, navigation tools, refactoring tools, safety tools, bash tools, conflict tools, AST tools, inspect tools, semantic tools, render helpers, diff-format helper, and the `callToolCall` transport wrapper (`packages/pi-plugin/src/tools/_shared.ts`)
- Depends on: `packages/aft-bridge/src/pool.ts`, `packages/pi-plugin/src/shared/`
- Used by: `packages/pi-plugin/src/index.ts`

**Protocol and command layer:**
- Purpose: Accept NDJSON requests, route tool calls via the unified `tool_call` command, and dispatch them to focused command handlers.
- Location: `crates/aft/src/main.rs`, `crates/aft/src/protocol.rs`, `crates/aft/src/commands/`, `crates/aft/src/commands/health_digest.rs`, `crates/aft/src/run_tool_call.rs`, `crates/aft/src/runtime_drain.rs`, `crates/aft/src/subc_translate.rs`, `crates/aft/src/subc_format.rs`, `crates/aft/src/gh_shim.rs`
- Contains: Request dispatch, response encoding, a unified `tool_call` routing engine, tool-to-command translation mapping, credential-free `gh` routing shim dispatch, server-rendered agent-facing text formatting (with directory outlines formatted as text unwrapping JSON envelopes), non-blocking control channel 0 health check responder reading derivation maps without spawning git subprocesses, and standalone command handlers for read/write/edit/hashline/apply_patch/delete_file/move_file/outline/zoom/bash/bash_orchestrate/bash_status/bash_wait_detach/bash_regex_match/batch/grep/glob/search/imports/refactor/LSP/inspect/conflicts/checkpoints/state/health_digest
- Depends on: `crates/aft/src/context.rs`, `crates/aft/src/parser.rs`, `crates/aft/src/callgraph.rs`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/edit.rs`, `crates/aft/src/semantic_index.rs`, `crates/aft/src/search_index.rs`, `crates/aft/src/compress/`
- Used by: `packages/aft-bridge/src/bridge.ts`

**Analysis and mutation engine layer:**
- Purpose: Parse code, compute call graphs, apply edits, format files, manage imports, index code semantically, and search with trigram indexes.
- Location: `crates/aft/src/cold_build_limiter.rs`, `crates/aft/src/parser.rs`, `crates/aft/src/callgraph.rs`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/callgraph_store/dead_code_projection.rs`, `crates/aft/src/edit.rs`, `crates/aft/src/format.rs`, `crates/aft/src/imports/`, `crates/aft/src/extract.rs`, `crates/aft/src/inspect/` (including `oxc_engine/`, `phase_log.rs`, and `scanners/`), `crates/aft/src/hashline/` (including `oracle/`), `crates/aft/src/semantic_index.rs`, `crates/aft/src/synapse_embed.rs`, `crates/aft/src/search_index.rs`, `crates/aft/src/grep_executor.rs`, `crates/aft/src/memory.rs`, `crates/aft/src/logging.rs`, `crates/aft/src/bash_rewrite/`, `crates/aft/src/symbols.rs`, `crates/aft/src/calls.rs`, `crates/aft/src/symbol_cache_disk.rs`, `crates/aft/src/symbol_diff.rs`, `crates/aft/src/fuzzy_match.rs`, `crates/aft/src/ast_grep_hints.rs`, `crates/aft/src/ast_grep_lang.rs`, `crates/aft/src/query_shape.rs`, `crates/aft/src/pattern_compile.rs`, `crates/aft/src/patch/`, `crates/aft/src/pty_render.rs`, `crates/aft/src/agent_child_env.rs`, `crates/aft/src/scoped_key.rs`, `crates/aft/src/walk_boundary.rs`
- Contains: Tree-sitter parsing using thread-local parsers (`REUSABLE_PARSERS`) to eliminate lock contention during parallel collection, optimized Rust symbol extraction traversing AST nodes directly without compiled queries, deterministic AST-backed symbol diffing between file revisions with indexed symbol line offsets (`crates/aft/src/symbol_diff.rs`), diff generation, formatter detection (resolving member-crate and Cargo editions for `rustfmt`, deferred out of configure warm paths), type-checker integration, import engines (Java, C#, PHP, Kotlin, Scala, Swift, Ruby, Lua, C/C++, Perl, Solidity, Vue, Groovy), refactor helpers, semantic embedding index (covering Java, Kotlin, Scala, Swift, Ruby, PHP, Lua, Perl, R, Objective-C, Groovy, and other supported languages), disk-backed trigram search index (supporting `build_denied` status for write-denied roots, copy-on-write search delta snapshots, and binary-search delta posting insertions), disk-backed symbol cache, persisted SQLite callgraph store builder (with cold builder slice fences and staging database resume across configure churn, snapshot-scoped `ModuleResolutionMemo` memoizing module specifiers and package manifests, Rust field callgraph resolution for direct `self` fields and method receiver whitespace, Rust function value reference extraction via `crates/aft/src/calls.rs` for reachability, and `test_origin` caller tracking to isolate test-only calls from production dead code), blocking-fresh inspect phase logging (`crates/aft/src/inspect/phase_log.rs`), process-wide cold build limiter with round-robin fair admission across request classes (`ColdBuildAdmissionClass`) and conditional wait predicates to reject slots for unbound roots, accelerated grep and glob query execution via `GrepExecutor` (with single-pass regex query decomposition `decompose_grep_pattern`, deterministic mtime path sorting with root-relative tiebreaks, and fallback walk limits and budgets when index is unavailable), process-wide memory attribution (reporting physical footprint `phys_footprint_bytes` on macOS to exclude `MADV_FREE` allocator noise), allocation pressure relief (running on idle sweeps and on periodic transport ticks when slack is >= 1 GiB under macOS and Linux), durable process log writing (`crates/aft/src/logging.rs`) enforcing PID-scoped log file rotation (32 MB cap, single backup generation), dead PID log reaping (24-hour quiet threshold), and total log directory storage budgeting (200 MB budget), UTC timestamps on file logs (`format_utc_timestamp`), AST-grep integration, patch parsing (Add, Delete, and Update hunks) and matching engine (resolving entry paths without following final symlinks for delete/move operations and refusing symlink deletions during patch preflight), streaming finite plain TOML output caps through sliding windows (`crates/aft/src/compress/toml_filter.rs`), edit failure steering diagnostics (`crates/aft/src/fuzzy_match.rs`) guiding agent repair attempts, pinned hashline engine (`crates/aft/src/hashline/`, covering byte scanning `scan/`, line-tag snapshot store `snapshot/`, syntax verification `syntax/`, PUT/CUT/REM apply repair `apply/`, two-phase transactions `transaction/`, remap recovery `recovery/`, transport integration `integration/`, release gates `release/`, and seed-zero xxHash32 oracle `oracle/`) computing seed-zero xxHash32 digests for hashline editing, vt100 terminal rendering for PTY screen snapshots, codebase-health scanners (with callgraph-blocked dead-code health status tracking, executing via a process-wide Rayon thread pool `INSPECT_THREAD_POOL` shared across roots), NestJS framework route and decorator spec entry point detection, same-file export liveness propagation, Go interface method liveness matching, manifest-derived signal tiering for ranking/down-ranking findings (Product, Test, Tooling) and generated-file filtering, bash rewrite rule decision tracking with observation logging (`crates/aft/src/bash_rewrite/`), standing index root key derivation (`crates/aft/src/scoped_key.rs`), and governed agent child environment injection (`crates/aft/src/agent_child_env.rs`) scrubbing SubC daemon credentials from child environments
- Depends on: tree-sitter grammars, ast-grep, vt100, external formatter and checker processes, ONNX Runtime (optional), fastembed / OpenAI-compatible / Ollama / Synapse backends (optional)
- Used by: `crates/aft/src/commands/*.rs`

**State and diagnostics layer:**
- Purpose: Hold per-process mutable state for backups, checkpoints, file watching, call graph cache, LSP state, database storage, bash background tasks, cache freshness tracking, file-system locking, and root-keyed writer leases.
- Location: `crates/aft/src/context.rs`, `crates/aft/src/backup.rs`, `crates/aft/src/checkpoint.rs`, `crates/aft/src/lsp/`, `crates/aft/src/db/`, `crates/aft/src/cache_freshness.rs`, `crates/aft/src/fs_lock.rs`, `crates/aft/src/bash_background/`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/response_finalize.rs`, `crates/aft/src/fleet_status.rs`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, `crates/aft/src/legacy_partitions.rs`, `crates/aft/src/alert_render.rs`, `crates/aft/src/alert_records.rs`, `crates/aft/src/alert_state.rs`, `crates/aft/src/agent_child_env.rs`, `crates/aft/src/scoped_key.rs`
- Contains: `AppContext` with symlink path verification checks (recursively following chain hops to reject escaping paths and memoizing canonicalized project roots in `path_restriction_root_memo` to minimize stat overhead), Windows verbatim path normalization via `canonicalize_normalized` to eliminate path comparison asymmetry, undo history, backup policies and disk-locking handlers, named checkpoints, watcher receiver, LSP manager (which resolves language servers across nested project/workspace virtualenv ladders in `crates/aft/src/lsp/registry.rs`, deduplicates analyzer roots by resolving to workspace `Cargo.toml` manifests in `crates/aft/src/lsp/roots.rs`, immediately reaps child processes rooted at worktrees with a sibling `.reclaimed` marker during maintenance sweeps in `crates/aft/src/lsp/child_registry.rs`, and lazily starts matching servers for explicit diagnostics requests), diagnostics store (which tracks and masks watcher-stale diagnostics for caching and pull reuse, promoting settled diagnostics upon rust-analyzer quiescence, and tracks newly-opened documents during pull diagnostics to automatically close them when the scoped collection drops while draining events to prevent queue buildup), document store, session-owned alert engine (`AlertEngine`) managing error diagnostic states and server-rendered `<system-reminder>` alerts, persistent database tables (backups, bash tasks, pattern watches via `crates/aft/src/db/bash_watches.rs`, compression events, state, standing roots via `crates/aft/src/db/standing_roots.rs`, callgraph edges and nodes, alert rendered records `alert_rendered_records`, and alert disappearance records `alert_disappearance_records`), cache-freshness tracker (which tracks file metadata and utilizes verification tickets to prevent race conditions during concurrent file invalidation between verify and completion steps), file-system lockfile (with process start-time and boot-ID liveness verification in `crates/aft/src/fs_lock.rs`), background task registry, PTY process pool, callgraph store background channels, main-loop pending responses registry, root-keyed writer leases and reader marker file coordination (protecting active reader processes from index removal), legacy harness coexistence guards (refusing writes into legacy layout partitions and validating migration space thresholds), `borrowed_index_cache` (which caches up to 4 read-only borrowed index search/semantic artifacts and resolved external git roots to optimize concurrent external search operations), and `worktree.ram_overlay` in-RAM trigram search and symbol cache delta tracking for borrow-only roots
- Depends on: `notify`, LSP transport helpers, Rust `RefCell`, SQLite (via `db/` and `callgraph_store/`), `serde`
- Used by: All command handlers through `AppContext`

## Data Flow

**Tool invocation flow:**

1. Register tool definitions and config-driven surface selection -- `packages/opencode-plugin/src/index.ts` or `packages/pi-plugin/src/index.ts`. Before establishing a bridge or transport, the plugin's `bridgeFor` entry point verifies that the target project root directory exists, immediately throwing an error if it has been deleted (such as a reclaimed worktree) to prevent configuring or warming indexes for a dead root.
2. Resolve the active transport pool:
   - For standalone mode (default): send a unified `tool_call` command carrying the bare tool name and arguments over NDJSON -- `packages/aft-bridge/src/pool.ts`, `packages/aft-bridge/src/bridge.ts`
   - For subc mode (when `subc.connection_file` is set): send `{name, arguments}` as a data-plane request over a tool-provider route channel opened and cached per session identity (`BindIdentity`) -- `packages/aft-bridge/src/subc-transport.ts`
3. Dispatch the request to the target command or executor. Under standalone mode, dispatch through the Rust stdin NDJSON loop. Under subc mode, process frames via the TCP loopback client loop. Local `configure` commands are satisfied locally on bind. Native plumbing tools (`bash_drain_completions`, `bash_ack_completions`, `bash_regex_match`) bypass the tool manifest check but reinject the BIND session ID to keep sessions isolated. The execution outcome is processed through the server-side text formatter (`crates/aft/src/subc_format.rs`) and a pending response finalizer seam (`crates/aft/src/response_finalize.rs`). Subc response frames contain `structuredContent` for first-party binds to re-lift the full flat response shape into `ToolCallResult` at the transport boundary, maintaining parity with standalone mode. For untrusted (MCP) binds, the server returns text-only replies (omitting `structuredContent` entirely) to prevent models like Claude Code from consuming raw JSON dumps and to save token costs. Monotonic phase traces (`PhaseTrace` and `ToolCallPhaseDurations`) track the timing/performance of subc tool calls across multiple phases (queuing, translation, execution, formatting, finalization, and egress) for slow-call diagnostics. Under subc mode, the initial attach loop retries transient connection and authentication failures (using an exponential backoff with jitter up to a 60-second budget) to recover from temporary daemon unavailability. Retry request dispatch once when a route is proven absent (receiving daemon `unknown_channel` or client `StaleRouteHandleError` before write). A cancelled route bind (e.g. Goodbye or deadline expiry) signals the configure job's cooperative `JobCancellation` handle; the running configure command checks this at phase boundaries (`configure_cancelled` and `root_commit_probe_cancelled`) to abort early and avoid building indexes or running git root commit probes for a dead route.

**Edit pipeline:**

1. Validate path and verify symlink safety (recursively follow components up to 40 hops to reject escaping paths), resolving relative paths against the bound project root via `AppContext::resolve_relative_path` before validation and safety keying -- `crates/aft/src/context.rs`
2. Translate tool arguments to command parameters -- `crates/aft/src/subc_translate.rs`. This includes resolving and normalizing path arguments. RFC 8089 `file:` URLs (e.g., `file:///path`, `file:/path`, or `file://localhost/path`) are percent-decoded using a byte-wise tolerant algorithm to ensure both the plugin-side permission check and the server-side resolution agree on the target filesystem path. When `edit_mode: "hashline"` is configured in `aft.jsonc`, `subc_translate.rs` routes edit requests to `handle_hashline_edit` (`crates/aft/src/commands/hashline.rs`) ahead of legacy shape checks, keying handles by symlink-resolved file identity to keep differing spellings in the same unit and executing mutations through the hashline two-phase transaction engine (`crates/aft/src/hashline/transaction/mod.rs`), where repeated sections for the same canonical path compose in patch order against pre-request coordinates into a single atomic transaction step. Tagged read rendering (`crates/aft/src/commands/read.rs`) mints line tags and snapshot baselines when hashline mode is active.
3. Check edit permissions -- `packages/opencode-plugin/src/tools/permissions.ts` (or Pi equivalents). Under Pi, project-internal mutations apply without confirmation prompts, while external paths are validated by Rust path restrictions to avoid unanswered-prompt hangs.
4. Snapshot, mutate, diff, and validate content -- `crates/aft/src/edit.rs`. When applying mutations via `apply_patch` (for delete and move hunks), `delete_file`, and `move_file`, entry paths are resolved without following final symlinks (with delete operations refusing symlink removal during preflight to protect un-undoable targets), preserving symlink identity for relative deletes and restricted moves. Line narrowing in `extract_function` and `inline_symbol` checks against positive `u32` overflow to reject malformed ranges with `invalid_request`.
5. Auto-format and optionally collect diagnostics after write -- `crates/aft/src/format.rs`, `crates/aft/src/context.rs`. By default, edits return immediately without waiting for LSP diagnostics; pass `diagnostics: true` to enable synchronous wait for diagnostics, or run `aft_inspect` (diagnostics category) to check them asynchronously. Explicit diagnostics-on-edit requests lazily start the matching LSP server so version-matched publishes are observed. Post-write LSP notifications are best-effort against running servers to avoid cold starts.

**Call-graph and navigation flow:**

1. Configure project root and initialize file watching -- `crates/aft/src/commands/configure.rs`. During configure maintenance sweeps, tool-cache invalidation is scoped to the reconfigured root via `clear_tool_cache_for_root` (`crates/aft/src/format.rs`) to prevent invalidating tool availability entries for other active project roots.
2. Query workspace-wide call dependencies via the persisted background-built callgraph store -- `crates/aft/src/callgraph_store/mod.rs`. The active database file is resolved dynamically via generation pointers (`.current` files) to allow atomic swaps and non-blocking reads. Cold builders stop at bounded slice fences (extraction, indexing, resolution, and dispatch) while retaining resumable staging databases so matching-corpus successors resume committed cursors or directly publish completed stages across configure churn, and staged resolution loops memoize positive/negative specifier targets, package manifests, and workspace roots via `ModuleResolutionMemo`. Under read-only mode or when writer capability is denied (borrow-only worktrees), writer-shaped callgraph APIs report terminal `callgraph_denied` unavailable status rather than unwrapping a read-only store into `CallGraphStore` or relaunching doomed cold builds, while queries run via `ReadonlyCallGraphStore` directly against the SQLite database without write or rebuild operations. Refresh writes are offloaded to a background worker thread (`aft-callgraph-refresh`) to prevent blocking the watcher and configure loops. Stale read-only search indexes are not skipped; they are still loaded to support prewarming the symbol cache.
3. Track and replay pending callgraph store paths waiting for a ready writable store. Paths outside the current project root are dropped to prevent indexing foreign files when a project root changes. The containment check (`pending_path_in_roots`) utilizes lenient, component-wise, filesystem-first canonicalization (`canonicalize_lenient`) on the nearest existing ancestor of a path to resolve alias spellings (like macOS `/var` vs `/private/var`) and deleted/missing files correctly.
4. Serve navigation commands such as callers, call-tree, impact, trace-to, and trace-data using the callgraph store adapter -- `crates/aft/src/commands/call_tree.rs`, `crates/aft/src/commands/callers.rs`, `crates/aft/src/commands/impact.rs`, `crates/aft/src/commands/trace_data.rs`, `crates/aft/src/commands/trace_to.rs`, `crates/aft/src/commands/trace_to_symbol.rs`, `crates/aft/src/commands/callgraph_store_adapter.rs`. By default, hide test files from results (controlled via the `includeTests` parameter) and collapse unresolved stdlib or external leaf calls in `call_tree` unless `includeUnresolved` is active. Truncate and return a summary (`hub_summary`) when results exceed 20 entries to save token context cost.
5. Serve symbol-level zoom inspection (`aft_zoom`), which fetches a symbol's implementation. If the target is a large container (class, struct, interface, etc., exceeding 150 lines), it renders a member-signature menu instead of the full body. For standard functions, it dedupes outgoing (`calls_out`) and incoming (`called_by`) call sites by name, aggregating duplicate occurrences under `extra_count` to minimize context token cost. For HTML and Markdown files, `aft_zoom` supports resolving explicit heading anchors (e.g. queries prefixed with `#` matching `id` or `name` attributes) without altering the layout outline -- `crates/aft/src/commands/zoom.rs`, `crates/aft/src/parser.rs`, `crates/aft/src/language.rs`.

**Search and retrieval flow:**

1. Index project files using a disk-backed, pread-based trigram search index that keeps memory overhead bounded -- `crates/aft/src/search_index.rs`. To prevent redundant disk hashing and index re-verification loops during configure bind/warmup sequences, a verification memo with a 10-minute TTL manages cache freshness checks, utilizing metadata stat checks (`VerifyStrategy::StatFirst`) when possible rather than strict content hashing. For grafted history roots, canonicalize the sorted, deduplicated set of root commits before hashing artifact keys to prevent Git traversal-order changes from triggering redundant index rebuilds.
2. Optionally index with dense embeddings (fastembed, OpenAI-compatible, Ollama, or Synapse over SubC) -- `crates/aft/src/semantic_index.rs`, `crates/aft/src/synapse_embed.rs`. Serialize cold semantic warmups by gating callgraph store building and Tier 2 diagnostics refreshes behind active cold semantic index seeds. Coalesce watcher-driven semantic re-embeds under a 15-second quiet window (`SEMANTIC_REFRESH_QUIET_WINDOW_MS`) to bundle edit bursts into a single collection pass, while masking changed files from search results until indexed to preserve query correctness. Reconfiguring semantic settings or project roots cancels superseded semantic builders while adopting matching live builders. In tests, override this quiet window via the `AFT_SEMANTIC_QUIET_WINDOW_MS` environment variable. Limit process-wide semantic refresh concurrency using the `ColdBuildLimiter` (sharing the slot budget with other heavy maintenance operations) to prevent concurrent background refreshes from overloading remote or local embedding backends -- `crates/aft/src/cold_build_limiter.rs`, `crates/aft/src/commands/configure.rs`.
3. Schedule standing-root search, semantic, and callgraph construction through the process-wide pressure-aware deficit round-robin scheduler -- `crates/aft/src/standing_scheduler.rs`, `crates/aft/src/resource_policy.rs`, `crates/aft/src/subc/standing.rs`. The scheduler admits at most the configured cold-build concurrency, rotates unfinished roots after each durable slice, and charges measured elapsed work against each root's deficit. Search, semantic, and callgraph builders persist versioned staging state and publish atomically only after the complete corpus is ready. The default `index.resource_policy = "balanced"` pauses new slices under battery saving or CPU, memory, and I/O pressure and resumes with hysteresis. `"performance"` bypasses resource admission for users who accept the power cost, but retains bounded concurrency, fair rotation, resumable checkpoints, and OS background thread priority.
4. Classify query shape (prose vs code) using the query shape parser -- `crates/aft/src/query_shape.rs`. Identify "type-concept identifier queries" (TitleCase PascalCase types combined with lowercase concepts) to trigger definition semantic priors.
5. Serve `grep` (trigram, full-text) and `aft_search` (semantic + hybrid) queries, delegating to `GrepExecutor` for accelerated path evaluation and enforcing execution safety limits (like `MAX_FALLBACK_WALK_FILES` and `FALLBACK_WALK_BUDGET`) during fallback walks when indexes are building or unavailable -- `crates/aft/src/grep_executor.rs`, `crates/aft/src/commands/grep.rs`, `crates/aft/src/commands/semantic_search.rs`. Under standalone bridge mode, interactive semantic searches support cancellable deferred polling in the main event loop. Borrow-only lexical and semantic snapshot opens bypass the cold-build limiter to prevent fresh-worktree search starvation while first searches wait cancellation-aware for a bounded loading window (2.5s). Interactive query embeddings and search artifact waits are bounded by dedicated budgets (`QueryBudget` and bounded interactive search artifact wait timeouts; `query_timeout_ms` clamped to 500..15000ms, defaulting to 3000ms) to keep interactive requests fast without affecting background build/refresh timeouts, falling back to lexical search if query embedding fails or times out. Downrank generated documentation artifacts (e.g. minified CSS/JS, maps, SVGs) in lexical and hybrid search results. For external search requests, resolve and cache external git roots, querying cached read-only search and semantic indexes from the `borrowed_index_cache` (capped at 4 concurrent entries) to avoid redundant git probes and disk parsing.

**File read flow:**

1. Map read arguments and validate boundary permissions -- `packages/opencode-plugin/src/tools/reading.ts`, `packages/pi-plugin/src/tools/reading.ts`. Under project-root path restriction, allow restricted reading of files outside the project root if they are session-owned bash artifact outputs (stdout, stderr, exit code, or pty outputs) registered under the requesting session ID (validated via `AppContext::validate_read_path` using `BgTaskRegistry::is_session_owned_artifact_path`), while strictly rejecting any mutations (which continue to enforce project root boundaries via `AppContext::validate_path`). The plugin skips the external-directory permission prompt for session-owned bash task artifacts under the AFT storage root when performing server-validated reads, avoiding hangs in unattended runs.
2. Sniff content type (text vs binary/PDF/image) and read contents -- `crates/aft/src/commands/read.rs`. For directory listings, `include_hidden` can be set (defaulting to true) to filter out hidden files/dotfiles, preserving the visibility filtering of transparent `ls` bash rewrites.
3. Process media attachments (resizing, orientation correction, and animation checks) and return them as base64-encoded attachment payloads alongside text content -- `crates/aft/src/commands/read.rs`, `crates/aft/src/subc_format.rs`

**Bash execution flow:**

1. Rewrite high-level commands (cat to read, grep to grep tool) evaluating rule branches, catalog control roles, and recording observation logs for differential campaigns -- `crates/aft/src/bash_rewrite/`
2. Scan for dangerous commands and prompt for permission -- `crates/aft/src/bash_permissions/`
3. Route first-party bash commands through the native platform sandbox (Landlock on Linux, Seatbelt on macOS) under `sandbox.enabled` -- `crates/aft/src/sandbox_spawn.rs`, `crates/aft/src/sandbox_profile.rs`, `crates/aft/src/cli/sandbox_launch.rs`. Sandbox configuration supports one-way project-tier hardening (allowing a repository to opt into sandboxing via `sandbox.enabled: true` in `.cortexkit/aft.jsonc`), while weakening configurations (`sandbox.enabled: false` and `write_allow`) are stripped as user-only. First-party principals can request a one-command escape by setting `sandbox: "host"`, which prompts the user with an `escalation` permission ask and a generated `grant_id` to run the command unsandboxed. When sandboxing is enabled on unsupported non-Unix platforms, execution fails closed with a `sandbox_unavailable` refusal. Sandboxed macOS Seatbelt profiles enforce explicit path deny rules for absent paths and deny secret-floor writes. Linux Landlock profiles invert read policy into an FD-anchored allowlist, refuse writable overlap with the credential floor, and document their alias, nested-write, Unix-socket, and `/proc` limits in [the sandbox platform matrix](docs/config.md#native-command-sandbox).
4. Provision governed agent child environment (`crates/aft/src/agent_child_env.rs`) for spawned bash/PTY processes, scrubbing SubC daemon credentials (`SUBC_SECRET` and SubC socket/token envs) from the child environment and populating a shims directory containing `gh` routing shim and a generated POSIX `prepare-commit-msg` git co-author hook (`AFT_GIT_CO_AUTHOR`), attaching attribution without altering user shell files or global git config. Non-PTY bash child processes are isolated in their own process session (`setsid()`) to prevent interactive shell job control against the harness terminal.
5. Execute foreground, background, PTY, or synchronous wait/foreground modes. Foreground bash executions are orchestrated with a wait window (defaulting to 15s, clamped to config) and deferred to background tasks if they exceed the budget, while wait mode (`wait: true`) blocks to completion using the timeout budget, but detaches to a background task immediately if a `bash_wait_detach` signal is received (such as when a new user message is processed, or when forced via the literal `&detach` token even if `bash.detach_on_user_message: false` is configured to prevent blocking agent interaction) -- `crates/aft/src/commands/bash_orchestrate.rs`, `crates/aft/src/commands/bash_status.rs`, `crates/aft/src/commands/bash_wait_detach.rs`, `crates/aft/src/pty_render.rs`, `crates/aft/src/bash_background/`. Background tasks preserve restart fate on daemon/bridge restarts (`BgTaskStatus::FateUnknown` when child exit was not cleanly observed) and protect recorded live processes against GC/quarantine by checking PID and process start-time liveness. If AFT bridge or daemon transport is unavailable, foreground commands can fall back to break-glass host fallback execution (`packages/aft-bridge/src/bash-host-fallback.ts`) prompting user confirmation via `bashHostFallbackAskPattern` and executing directly without rewrites, compression, or background tasks.
6. Compress output through the tiered compressor -- `crates/aft/src/compress/`

**Background completion wake flow:**

1. Maintain background subscriptions for completions. Under standalone mode, completion notifications push directly over the bridge process stdout channel. Under subc mode, the plugin maintains a persistent `BgSubscription` over a dedicated second route channel -- `packages/aft-bridge/src/subc-transport.ts`.
2. When a background task completes, Rust marks the session's background channel wake-pending using an epoch-based tracking mechanism to prevent race conditions during concurrent tool/maintenance execution (i.e. to avoid suppressing wakes armed after a maintenance snapshot). It emits a coalesced, lossy `{op: "bg_events"}` wake nudge at most once per 250ms tick -- `crates/aft/src/subc/mod.rs`.
3. The plugin receives the nudge via `onBgEventsNudge` and triggers an unconditional forced-drain (`handleSubcBgEventsNudge`) to fetch, deliver, and ack the completions -- `packages/opencode-plugin/src/bg-notifications.ts`, `packages/pi-plugin/src/bg-notifications.ts`. To prevent double-delivery during concurrent tool/forced-drain execution, the plugin maintains two transient per-session task-ID tracking sets: `deliveringTaskIds` (delivery in flight) and `deliveredAwaitingAckTaskIds` (delivered but unacknowledged). Forced drains skip tasks in either set, and automatically re-ack tasks in the awaiting-ack set to terminate subc re-nudge loops. The plugin uses daemon reconciliation (rather than a static time-based TTL) to prune `deliveredAwaitingAckTaskIds`, removing tasks only when they are no longer returned in the daemon's list of outstanding tasks.
4. If a subc background subscription channel drops, `BgSubscription` drives its own independent reconnect loop to resubscribe without waiting for new tool traffic, retrieving any completions queued while disconnected.

**Binary resolution flow:**

1. Check cache, npm platform package, PATH, and cargo install locations -- `packages/aft-bridge/src/resolver.ts`
2. Download and checksum-verify a release asset when local resolution fails -- `packages/aft-bridge/src/downloader.ts`
3. Start bridges against the resolved binary and hot-swap after version mismatch -- `packages/aft-bridge/src/bridge.ts`, `packages/aft-bridge/src/pool.ts`

**Artifact ownership and read-only caching flow:**

1. During `configure`, verify repository scopes, and resolve root-keyed cache directories: `<storage>/callgraph/<artifact_cache_key>` and `<storage>/inspect/<project_scope_key>` -- `crates/aft/src/commands/configure.rs`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/root_cache.rs`. If the `storage_dir` parameter is not supplied (e.g. daemon connections), default it to the shared CortexKit storage root `~/.local/share/cortexkit/aft/` to prevent RAM-only fallback and index regeneration. Retrieve or record the cache key in `<storage>/cache-keys.json` to memoize the mapping and avoid spawning redundant git process probes. During `configure` (client bind), heavy artifact index loading (deserialization) is deferred until after the bind acknowledgement has been returned to the client, kicking off only during the post-bind maintenance sweep (`drain_deferred_configure_maintenance`).
2. Write an `owner.json` manifest to the cache directory carrying the current checkout's scope key, path, PID, and hostname.
3. If no manifest exists, or if the existing manifest belongs to the same checkout, or if the owning process is dead (stale heartbeat or inactive process ID/hostname), reclaim and write a new lease ("Owner" mode).
4. If an active process on another checkout owns the manifest, claim "ReadOnly" mode.
5. Coordinate concurrent cache writes by acquiring a domain-specific `WriterLease` via `WriterLease::acquire_shared`. In "ReadOnly" mode or when a write lease cannot be acquired, heavy operations like cold callgraph builds, search index generation, and semantic index warming are disabled. Any search or semantic search queries read the cached index files using strict read-only openers (including `ReadonlyCallGraphStore` for callgraph reads) -- `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, `crates/aft/src/callgraph_store/mod.rs`. Borrow-only roots (e.g., mason worktrees) report search and callgraph indexes as ready since they query shared read-only artifacts, and can opt into `worktree.ram_overlay` to apply watcher events to in-RAM trigram search index deltas and symbol caches so local edits are searchable without persisting to the shared disk cache. Subc health reports only mark the daemon degraded when dispatch is impaired (e.g., actor snapshot contention), leaving roots warming background indexes or serving read-only work with an Ok health verdict -- `crates/aft/src/subc/health.rs`.
6. Write a `0600` read-marker file under `<domain>/readers/<generation-label>/` to register active reader sessions -- `crates/aft/src/root_cache.rs`. Active readers touch their markers to update heartbeats (at most once every 5 seconds). During sweeps, garbage collection removes old generation SQLite databases unless they have a protected read marker, or if their age exceeds the absolute retention limit of 6 hours (`MARKED_GENERATION_RETENTION_TTL`).
7. Enforce coexistence guards using `legacy_partitions.rs` to refuse write operations into legacy layout partitions and check free space requirements using a 1.5× disk-floor preflight before copying legacy folders to the new root-keyed layout. If a legacy callgraph partition is detected, migrate it using the non-blocking, online SQLite backup API (`rusqlite::Connection::backup`) running page-by-page (128 pages per step) under retry and wall-clock budgets.
8. The active "Owner" session emits periodic heartbeat file-writes to the lease manifest file during its event loop tick -- `crates/aft/src/main.rs`.

**Idle root eviction, unbound quiescing, and memory management flow:**

1. Track the idle time of active project roots based on request activity. When a root has been idle for a configured TTL (`IDLE_ROOT_TTL`), the subc maintenance loop triggers eviction -- `crates/aft/src/subc/mod.rs`, `crates/aft/src/context.rs`.
2. When a project root becomes unbound (no active routes/channels remaining and no pending binds), the subc daemon quiesces it: marks the actor context as subc unbound, invalidates the configure generation, retires search/callgraph/semantic build receivers, cancels queued and pending artifact work, cancels all queued maintenance jobs (returning `"maintenance_cancelled"` answers, except for active `Lsp` drains which are allowed to run/finish), and discards deferred configure maintenance. Transient unbind deliberately keeps the watcher and resident artifacts warm so a host restart can rebind without a full verification scan. Receiver generation/epoch pairs prevent already-dequeued results from committing after teardown or replacement, while per-artifact publication epochs prevent superseded workers from publishing stale disk pointers. When a new route is bound, the root is reactivated, clearing the quiesced and evicted flags.
3. After the idle TTL, and only while the root still has no bound or pending route, evict root-scoped artifact handles (callgraph store, search index, semantic index, borrowed indexes, symbol data, and inspect SQLite caches) via `evict_idle_artifacts`; stop and bounded-join the watcher on a detached reaper thread; and shut down reopenable LSP clients in the background. Subsequent queries trigger asynchronous index reloads. Because edits during watcher downtime go unobserved, advance artifact publication epochs and invalidate the verify memo, forcing `WarmVerifyPlan::Strict` re-verification on a later bind. The process-wide tree-sitter parser cache and shared `aft.db` connection are not per-root resources.
4. If the unbound root directory no longer exists, remove its idle executor actor and drop its LSP, bash watchdog, channels, and registries on a detached teardown thread. Purge detached-session replay and wake state for that root; a missing-directory root cannot be rebound by the plugin. If cleanup of an idle or deleted root is blocked, a detailed reap blocker census (`ReapBlockerCensus`) tracks and exposes the specific blockers (such as active route channels, quiescing status, background bash waits, or pending/queued maintenance tasks) within the subc health report -- `crates/aft/src/subc/health.rs`.
5. Under macOS and Linux, after sweeping idle roots or periodically on transport ticks when reported allocator slack is >= 1 GiB, request memory pressure relief from the OS allocator via `relieve_allocator_pressure` to reclaim unused pages -- `crates/aft/src/memory.rs`.
6. Track process-wide and root-scoped memory usage (including SQLite allocator metrics and OS RSS memory) via memory snapshots returned in status reports -- `crates/aft/src/memory.rs`, `crates/aft/src/commands/status.rs`. Status runtime counts expose live watcher runtimes, live actor roots, and open routes. Key status memory roots by `ProjectRootId` on all platforms to prevent path-casing/verbatim comparison mismatches. To prevent large status payloads from exceeding metrics cache limits, the per-root detail breakdown in status payloads and health check metrics is capped (e.g. at the top 8 roots by attributed bytes), and the remaining entries are rolled up in a compact summarized footprint -- `crates/aft/src/subc/health.rs`, `crates/aft/src/memory.rs`.

**Codebase inspection flow:**

1. Receive `aft_inspect` tool request -- `packages/opencode-plugin/src/tools/inspect.ts`, `packages/pi-plugin/src/tools/inspect.ts`, `crates/aft/src/commands/inspect.rs`.
2. Blocking inspections execute full-root analysis across all active categories (LSP diagnostics, Tier 2 rescans, callgraph store readiness, and stat verification), bounded by phase deadlines and responsive to cooperative cancellation tokens (`JobCancellation`), recording completed phases into an `InspectPhaseLog` (`crates/aft/src/inspect/phase_log.rs`).
3. If an explicit `scope` is provided, it narrows the returned drill-down findings but does not reduce full-root verification work. `sections` controls which categories are rendered in detail versus summarized.
4. Producer-scoped LSP start or spawn failures isolate into reportable named gaps (`ApplicableServerFailure`) in the diagnostics category payload without aborting the overall inspection request, preserving verified results from surviving producers.
5. Return a terminal inspection result (`FRESH` carrying completed phases and a wait-stamp; `INTERRUPTED`; or `PHASE-FAILED` carrying failure reason, failure detail, and completed phases).

**Alert observation and response finalization flow:**

1. Ingest authoritative LSP diagnostic snapshots per producer partition (`ProducerKey`) -- `crates/aft/src/alert_state.rs`.
2. The session-owned alert engine (`AlertEngine` in `crates/aft/src/alert_render.rs`) tracks active and newly surfaced error diagnostics per dispatch root across agent turns.
3. During response finalization (`crates/aft/src/response_finalize.rs::finalize_response_for_dispatch_root`), attach server-rendered `<system-reminder>` alert blocks containing error findings and counts to agent-visible tool responses, self-labeling AFT's status bar segment (`AFT E... W... | ...`) for fleet status composition.
4. For OpenCode sessions, write durable observation rows (`alert_rendered_records`) and disappearance rows (`alert_disappearance_records`) into SQLite (`crates/aft/src/alert_records.rs`) to track 5-turn resolution lifecycles.

## Key Abstractions

**BinaryBridge:**
- Purpose: Keep one live `aft` subprocess available for request/response traffic.
- Location: `packages/aft-bridge/src/bridge.ts`
- Pattern: Persistent child-process adapter with timeout-triggered restart

**BridgePool:**
- Purpose: Scope bridges per OpenCode/Pi session and preserve isolated undo history.
- Location: `packages/aft-bridge/src/pool.ts`
- Pattern: Session-keyed object pool with LRU eviction

**RevivableTransportPool / RevivableProjectTransport:**
- Purpose: Wrap active transport pools and project transports to handle post-shutdown revival without reusing dead routes, sessions, or sockets.
- Location: `packages/aft-bridge/src/revivable-transport.ts`
- Pattern: Facade wrapper owning a terminal transport pool that automatically instantiates a new active pool on demand when new request traffic arrives after host shutdown hooks.

**AftTransportPool / AftProjectTransport / AftTransport:**
- Purpose: Abstract transport details (standalone NDJSON vs daemon-backed subc) behind a unified, session-closed client-facing interface.
- Location: `packages/aft-bridge/src/transport.ts`, `packages/aft-bridge/src/transport-factory.ts`
- Pattern: Factory-created abstraction layer.

**SubcTransportPool:**
- Purpose: Provide route cache and connection management over the authenticated subc client.
- Location: `packages/aft-bridge/src/subc-transport.ts`
- Pattern: Cache per-identity session lifecycle records (`SessionRecord`) containing tool route entries, background event subscriptions (`BgSubscription`), opaque background nudge references (`BgNudgeRef`), closed states, and in-flight request counts. Force single-flight connection/route opening (preventing duplicate channel leaks) and handle safe session teardown by executing synchronous state mutations before any asynchronous transport cleanup to prevent in-flight request resurrection. Feature a client-level half-open backstop that drops/reconnects the client after consecutive non-transient request failures (e.g. timeouts) to recover from silent connection drops. Accept a test or in-process override for route principal identity (`consumerIdentity`). Retries request dispatch once when a route is proven absent (receiving daemon `unknown_channel` or client `StaleRouteHandleError` before write).

**BgSubscription:**
- Purpose: Consume the daemon's held-open `bg_events` wake lane.
- Location: `packages/aft-bridge/src/subc-transport.ts`
- Pattern: Resubscribe itself independently on stream drop or error without waiting for tool traffic, driving unconditional forced-drains.

**BindTrust:**
- Purpose: Enforce caller-identity (principal) trust levels on the subconscious routing daemon connection.
- Location: `crates/aft/src/subc/mod.rs`
- Pattern: Map route binds onto `FirstParty` or `Untrusted` levels by inspecting the caller's principal metadata. `Principal::Direct` and reserved module principals (`llm-runner`, `aft`, `broca`, `alfonso-core`, `prefrontal`, `prefrontal-core`) resolve to `FirstParty` trust. Other callers (e.g., facade `subc-mcp` module, unverified principal, or absent principal) map to `Untrusted` trust. `Untrusted` routes deny bash/shell executions, force project-root path restriction check validation even if globally disabled in user config, and block background task observation/wake replay.

**Tool groups (OpenCode):**
- Purpose: Group related OpenCode tool definitions by capability surface.
- Location: `packages/opencode-plugin/src/tools/hoisted.ts`, `packages/opencode-plugin/src/tools/reading.ts`, `packages/opencode-plugin/src/tools/imports.ts`, `packages/opencode-plugin/src/tools/navigation.ts`, `packages/opencode-plugin/src/tools/refactoring.ts`, `packages/opencode-plugin/src/tools/safety.ts`, `packages/opencode-plugin/src/tools/conflicts.ts`, `packages/opencode-plugin/src/tools/ast.ts`, `packages/opencode-plugin/src/tools/bash.ts`, `packages/opencode-plugin/src/tools/bash_watch.ts`, `packages/opencode-plugin/src/tools/bash_write.ts`, `packages/opencode-plugin/src/tools/inspect.ts`, `packages/opencode-plugin/src/tools/search.ts`, `packages/opencode-plugin/src/tools/semantic.ts`, `packages/opencode-plugin/src/tools/permissions.ts`, `packages/opencode-plugin/src/tools/hoisted-internals.ts`
- Pattern: Thin TypeScript adapters delegating to the unified `tool_call` transport

**Tool groups (Pi):**
- Purpose: Group related Pi tool definitions by capability surface.
- Location: `packages/pi-plugin/src/tools/hoisted.ts`, `packages/pi-plugin/src/tools/reading.ts`, `packages/pi-plugin/src/tools/imports.ts`, `packages/pi-plugin/src/tools/navigate.ts`, `packages/pi-plugin/src/tools/refactor.ts`, `packages/pi-plugin/src/tools/safety.ts`, `packages/pi-plugin/src/tools/conflicts.ts`, `packages/pi-plugin/src/tools/ast.ts`, `packages/pi-plugin/src/tools/bash.ts`, `packages/pi-plugin/src/tools/semantic.ts`, `packages/pi-plugin/src/tools/inspect.ts`, `packages/pi-plugin/src/tools/fs.ts`, `packages/pi-plugin/src/tools/diff-format.ts`, `packages/pi-plugin/src/tools/render-helpers.ts`
- Pattern: Thin TypeScript adapters delegating to the unified `tool_call` transport with Pi-specific schema configuration

**ToolCallCommand:**
- Purpose: Route and execute client-facing agent tools via a single request.
- Location: `crates/aft/src/commands/tool_call.rs`, `crates/aft/src/run_tool_call.rs`
- Pattern: Unified request translator and response formatting coordinator
- Contains: `subc_translate` mapping, `subc_format` text rendering, and dispatching to target command handlers

**Executor:**
- Purpose: Schedule and serialize interactive commands and background maintenance tasks.
- Location: `crates/aft/src/executor/mod.rs`
- Pattern: Multi-lane priority scheduler executing jobs concurrently across lanes (e.g. mutating, LSP, reads) using a worker thread pool. It handles actor lifecycle transitions, enforces per-actor maintenance queue capacity (`MAINTENANCE_QUEUE_CAP = 512`) returning `maintenance_backpressure` on overflow, deduplicates idempotent maintenance tasks via `MaintenanceCoalesceKey` (`WatcherDrain`, `LspDrain`), batches scheduler events (`SCHEDULER_EVENT_BATCH_CAP = 64`) to yield scheduler lock turns between batches, and supports queue cancellation when actors are unbound.

**JobCancellation:**
- Purpose: Provide cooperative cancellation for running configure operations and interactive requests.
- Location: `crates/aft/src/executor/mod.rs`
- Pattern: Atomic state machine (pending, running, committed, cancelled) shared between the scheduler and command runners, checked at phase boundaries (`configure_cancelled`) to abort execution early.

**AppContext:**
- Purpose: Centralize runtime state for commands inside the Rust worker.
- Location: `crates/aft/src/context.rs`
- Pattern: Interior-mutable service container for a single-threaded request loop
- Contains: `CallGraph`, `CallGraphStore`, `SearchIndex`, `SemanticIndex`, `BgTaskRegistry`, `FilterRegistry`, database connections, LSP manager, undo history

**CallGraphStore:**
- Purpose: Persisted SQLite database of project-wide call dependencies.
- Location: `crates/aft/src/callgraph_store/mod.rs`
- Pattern: Background-built SQLite schema containing resolved and name-only call edges, refreshed incrementally on file edits, and queried by navigation commands. Stores are resolved dynamically via generation pointers (`.current` files) to allow safe atomic swaps and non-blocking reads. Cold builders stop at bounded slice fences (extraction, indexing, resolution, and dispatch) while retaining resumable staging databases so matching-corpus successors resume committed cursors or directly publish completed stages, and module resolution across staged windows is memoized via `ModuleResolutionMemo`. Under read-only mode, queries read the SQLite file via `ReadonlyCallGraphStore` to prevent write collisions. Returns a `Building` status during cold builds. Cold-build warming is deferred while a cold semantic index seed is actively collecting or embedding.

**Oxc Liveness Engine:**
- Purpose: Perform liveness and dead-code analysis for JavaScript/TypeScript and other supported files.
- Location: `crates/aft/src/inspect/oxc_engine/`
- Pattern: Build a liveness graph starting from resolved entry points. Features include:
  - Framework-decorator roots: Seeding NestJS `@Controller` or `@Injectable` exports as live.
  - Same-file value reference propagation: Keeping exported helpers live when they are referenced locally in the same file, even if the file is not reached from a global entry point.
  - Language-specific dispatch rules: Keeping Go interface methods and receiver/interface dispatches live by matching method names to keep their bodies reachable.

**ProjectRoles / SignalTier:**
- Purpose: Rank inspect findings to prioritize actionable product code over tests, tooling, and generated artifacts.
- Location: `crates/aft/src/inspect/entry_points.rs`, `crates/aft/src/inspect/generated.rs`
- Pattern: Manifest-derived classification mapping files onto `Product`, `Test`, or `Tooling` tiers, combining with generated-file marker detection to sort high-signal findings first in `aft_inspect` summary previews and drill-downs.

**CallGraph:**
- Purpose: Cache per-file local call data and resolve immediate import edges.
- Location: `crates/aft/src/callgraph.rs`
- Pattern: Lazy workspace index with invalidation on watcher events.

**SearchIndex:**
- Purpose: Provide fast trigram-based full-text search across the project.
- Location: `crates/aft/src/search_index.rs`
- Pattern: Disk-backed (pread) postings index written to a single cache file (`cache.bin`) and read on-demand to maintain a bounded RAM footprint, rebuilding in the background on watcher events.

**SemanticIndex:**
- Purpose: Provide dense-embedding semantic search across the project.
- Location: `crates/aft/src/semantic_index.rs`, `crates/aft/src/synapse_embed.rs`
- Pattern: Optional index backed by fastembed (local), OpenAI-compatible, Ollama, or Synapse (over SubC daemon route); configurable `max_files` cap

**BgTaskRegistry:**
- Purpose: Manage background bash tasks and PTY sessions.
- Location: `crates/aft/src/bash_background/registry.rs`
- Pattern: Thread-safe registry with a watchdog thread for output compression, completion notification, and task lifecycle cleanup. Generate unique task IDs using 64-bit entropy (represented as a 16-hex character slug `bash-{16hex}`) to prevent ID reuse collisions during subc delivery de-duplication. Preserves restart fate (`BgTaskStatus::FateUnknown`) across daemon restarts when child exit is unobserved, checks recorded process start-time liveness before GC or quarantine, and retargets completion delivery to active sessions.

**Compressor:**
- Purpose: Reduce hoisted-bash output to relevant tokens.
- Location: `crates/aft/src/compress/` (multiple modules), `crates/aft/src/compress/mod.rs`
- Pattern: Trait-based dispatch with per-command Rust modules, output-shape sniffers, package-manager modules, declarative TOML filters, and a generic fallback

**PendingResponses:**
- Purpose: Hold and poll deferred or orchestrated requests in the main loop.
- Location: `crates/aft/src/response_finalize.rs`, `crates/aft/src/main.rs`
- Pattern: Vector-backed pending queue that polls registered completion steps and runs the finalizer seam before writing responses.

**PatchEngine:**
- Purpose: Parse, match, and apply unified file diffs/patches.
- Location: `crates/aft/src/patch/` (including `mod.rs`, `parser.rs`, `matcher.rs`, `apply.rs`)
- Pattern: AST/line-based parser that maps update/create/delete hunks to target files, matches fuzzy sequences, and executes atomic writes with rollback support.

**PtyRenderer:**
- Purpose: Render raw PTY output bytes into a readable screen.
- Location: `crates/aft/src/pty_render.rs`
- Pattern: vt100 terminal state parser that outputs clean, grid-aligned text for screen snapshots.

**DurableLogger:**
- Purpose: Manage PID-scoped log file creation, log rotation, dead-process log reaping, and log directory storage budgeting.
- Location: `crates/aft/src/logging.rs`, `packages/aft-bridge/src/durable-log.ts`
- Pattern: Asynchronous log writer enforcing a 32 MB per-file cap (retaining 1 rotated backup generation), reaping dead PID log files after a 24-hour quiet window, and maintaining a 200 MB maximum directory storage budget.

**Harness:**
- Purpose: Represent the coding-agent harness (OpenCode or Pi) for config and CLI dispatch.
- Location: `crates/aft/src/harness.rs`
- Pattern: Simple enum with serde round-trip and display/from-str

**ArtifactOwnerLease / ArtifactOwnerClaim:**
- Purpose: Prevent concurrent AFT processes from corrupting shared cache artifacts for the same repository while allowing safe read-only fallbacks.
- Location: `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`
- Pattern: File-system lease with active process liveness tracking.
- Contains: Unique process identification (`pid`, `hostname`), heartbeat updates during the event loop to preserve the lease, stale lease reclamation, and read-only index adapters that query the cached search and semantic indexes without trigger-building or modifying files.

**WriterLease / ReadMarker:**
- Purpose: Coordinate safe multi-session access to the root-keyed project cache.
- Location: `crates/aft/src/root_cache.rs`
- Pattern: File-locked writer lease ensuring single-writer exclusivity combined with private read-marker JSON files (created `0600` under `<domain>/readers/`) to track active reader processes.
- Contains: `WriterLease` domain mapping (`RootCacheDomain::Callgraph` or `RootCacheDomain::Inspect`), epoch validation, and process identification metadata for reader heartbeat cleanup. Sweeps clean up dead-PID and expired cross-host markers and delete old SQLite generation files.

**ArtifactPublishEpoch:**
- Purpose: Prevent check-then-publish race conditions during concurrent background artifact generation.
- Location: `crates/aft/src/root_cache.rs`
- Pattern: Serializes artifact supersession with the final disk publication step, ensuring that old or superseded workers do not publish stale disk pointers after a new configure generation has started.

**LegacyPartitionGuards:**
- Purpose: Protect legacy harness-scoped folders and manage space limits during layout migration.
- Location: `crates/aft/src/legacy_partitions.rs`
- Pattern: Coexistence write filters and space preflight check.
- Contains: Layout checkers preventing writes to legacy partition structures, disk space size scanning, and 1.5× disk-floor verification before copying files to the new root-keyed cache location.

**GrepExecutor:**
- Purpose: Coordinate accelerated search query execution across project roots and external directories.
- Location: `crates/aft/src/grep_executor.rs`
- Pattern: Query router and execution planner.
- Contains: In-index search routing combined with fallback manual walks capped by size (`MAX_FALLBACK_WALK_FILES`) and time budgets (`FALLBACK_WALK_BUDGET`) when indexes are building or unavailable.

**MemoryEstimate / MemorySnapshot:**
- Purpose: Track, attribute, and report process-wide and subsystem-specific memory usage.
- Location: `crates/aft/src/memory.rs`
- Pattern: Diagnostic structures and OS memory allocators hook.
- Contains: Subsystem memory estimation helpers, SQLite allocator query bindings (`sqlite3_memory_used`), platform-specific resident set size (RSS) and macOS kernel physical footprint (`phys_footprint_bytes` via `proc_pid_rusage RUSAGE_INFO_V4`) queries to exclude `MADV_FREE` allocator noise, and macOS-specific pressure relief bindings (`malloc_zone_pressure_relief`) to release unused pages during idle sweeps and periodic ticks.

**FleetStatusClient:**
- Purpose: Publish AFT's project-scoped status segment to the fleet status-holder plane (`prefrontal-core`).
- Location: `crates/aft/src/fleet_status.rs`
- Pattern: Publisher channel client targeting the status holder over `status.line` operations with rate-limiting cadence (2.5s) and fence-backed revision acknowledgements. When the status-holder route is live on OpenCode, it publishes status segments and suppresses local response status-bar attachment.

**AlertEngine / AlertRecords:**
- Purpose: Ingest authoritative LSP diagnostic observations, track alert lifecycles per session and dispatch root, format server-rendered `<system-reminder>` alerts on response finalization, and persist durable observation/disappearance records in SQLite.
- Location: `crates/aft/src/alert_render.rs`, `crates/aft/src/alert_records.rs`, `crates/aft/src/alert_state.rs`
- Pattern: Session-owned delta engine maintaining active error diagnostic sets per producer partition (`ProducerKey`), rendering short attribution lines with ellipsis caps (`MAX_ALERT_LINE_CHARS = 240`, `MAX_RENDERED_ALERT_LINES = 3`), and recording five-turn lifecycle entries into `alert_rendered_records` and `alert_disappearance_records`.

**SymbolDiff:**
- Purpose: Deterministic AST-backed symbol diffing between file revisions.
- Location: `crates/aft/src/symbol_diff.rs`
- Pattern: Compares two file byte slices using Tree-sitter parsers, indexing symbol line offsets to classify symbol changes into added, removed, and modified entries along with line-count deltas for code files while treating JSON/YAML as data files.

**HealthDigest:**
- Purpose: Provide a passive management operation returning freshness-ticketed current health values.
- Location: `crates/aft/src/commands/health_digest.rs`
- Pattern: Management operation (`health.digest`) without agent tool registration, returning ticketed current values (`FreshnessTicket::DocumentVersion`, `ArtifactGeneration`, `WatcherJournal`) for errors, dead code, unused exports, duplicates, todos, and watcher events.

**BashHostFallback:**
- Purpose: Provide break-glass direct foreground shell execution when AFT bridge or daemon transport is unavailable.
- Location: `packages/aft-bridge/src/bash-host-fallback.ts`
- Pattern: Standalone foreground child-process execution requiring explicit user confirmation via `bashHostFallbackAskPattern` and capping output at 100 KB (`BASH_HOST_FALLBACK_MAX_OUTPUT_BYTES`) and timeout at 10 minutes.

**NpmResolver:**
- Purpose: Resolve `npm` executable and augment spawn environment when launched from PATH-stripped GUI environments.
- Location: `packages/aft-bridge/src/npm-resolver.ts`
- Pattern: Resolves system, version manager (nvm, mise, volta, fnm, asdf), and Homebrew npm binaries, augments `PATH` with sibling `node` directories, handles Windows `npm.cmd` invocations via `cmd.exe`, and manages process-tree termination (`terminateNpmProcessTree`).

**InspectPhaseLog:**
- Purpose: Track completed work units during blocking-fresh codebase health inspections.
- Location: `crates/aft/src/inspect/phase_log.rs`
- Pattern: Per-request phase journal recording completed phases (`InspectPhaseId`: `LspStart`, `LspQuiescence`, `Tier2Rescan`, `CallgraphReady`, `StatVerification`) and attributing them to producers or categories to form terminal inspection payloads (`FRESH`, `INTERRUPTED`, `PHASE-FAILED`).

**GhShim:**
- Purpose: Route `gh` CLI invocations without ambient credentials.
- Location: `crates/aft/src/gh_shim.rs`
- Pattern: Process boundary intercepting `gh` command invocations, validating signed routing manifests (`gh-routing-manifest`), preserving signed reasoning prose across cache parses, supporting governed author comment edits (`--edit-last`), routing governed commands to `prefrontal-core` via subc `gh.route` seam or execing upstream `gh` for un-governed/mechanical operations (yielding refusal exit status 86 on security/manifest violations), and providing offline self-report (`--status`, `--shim-version`).

**AgentChildEnv:**
- Purpose: Inject governed execution environment and shims for first-party spawned agent child processes.
- Location: `crates/aft/src/agent_child_env.rs`
- Pattern: Shims directory manager and environment builder provisioning `gh` routing shim, scrubbing SubC daemon credentials (`SUBC_SECRET` and SubC socket/token envs) from tool child environments, and generating a POSIX `prepare-commit-msg` hook for joint co-authorship attribution (`AFT_GIT_CO_AUTHOR`) without modifying repository or global Git configuration.

**ScopedKey / StandingRoots:**
- Purpose: Manage isolated artifact identities and durable resolution for standing index roots pointing below a Git worktree.
- Location: `crates/aft/src/scoped_key.rs`, `crates/aft/src/db/standing_roots.rs`
- Pattern: Canonical `scoped-v1` artifact key derivation and machine-scoped SQLite registration preventing subtree roots from reusing repository-wide session keys.

**SynapseEmbeddingClient:**
- Purpose: Provide dense-embedding semantic search over the SubC daemon transport.
- Location: `crates/aft/src/synapse_embed.rs`
- Pattern: SubC client adapter forwarding `embed.query` and `embed.batch` requests to the Synapse module, validating payload content hashes, model certification, table epochs, and vector dimensions.

**DeviceBoundary:**
- Purpose: Prevent recursive directory walks from crossing filesystem mounts.
- Location: `crates/aft/src/walk_boundary.rs`
- Pattern: Captures root filesystem device ID (`MetadataExt::dev` on Unix) and verifies child directory device IDs before descending to prevent `ReadDir` drop panics when mounted volumes disappear.

## Entry Points

**OpenCode plugin entry point:**
- Location: `packages/opencode-plugin/src/index.ts`
- Triggers: OpenCode loads the `@cortexkit/aft-opencode` plugin
- Responsibilities: Load config, resolve the binary via `@cortexkit/aft-bridge`, create the bridge pool, register tool definitions, manage session lifecycle, run auto-update checker, handle background completion push frames

**Pi plugin entry point:**
- Location: `packages/pi-plugin/src/index.ts`
- Triggers: Pi loads the `@cortexkit/aft-pi` plugin
- Responsibilities: Load config, resolve the binary via `@cortexkit/aft-bridge`, create the bridge pool, register tool definitions, manage LSP auto-install (npm + GitHub), handle background completion push frames

**Unified CLI entry point:**
- Location: `packages/aft-cli/src/index.ts`
- Triggers: `npx @cortexkit/aft` invocation
- Responsibilities: Parse argv, auto-detect harness, dispatch to `setup`, `doctor`, or `doctor lsp` commands

**Shared bridge entry point:**
- Location: `packages/aft-bridge/src/index.ts`
- Triggers: Imported by `@cortexkit/aft-opencode` and `@cortexkit/aft-pi`
- Responsibilities: Export `BinaryBridge`, `BridgePool`, binary resolution (`downloadBinary`, `ensureBinary`, `findBinary`), ONNX runtime detection (`ensureOnnxRuntime`, `isOrtAutoDownloadSupported`), storage migration (`ensureStorageMigrated`), compact formatting helpers

**Rust protocol entry point:**
- Location: `crates/aft/src/main.rs`
- Triggers: `packages/aft-bridge/src/bridge.ts` spawns the `aft` binary
- Responsibilities: Read NDJSON requests from stdin, dispatch handlers, drain watcher and LSP events, compress background task output, and write JSON responses

**Rust subc daemon entry point:**
- Location: `crates/aft/src/main.rs`, `crates/aft/src/subc/mod.rs`
- Triggers: Spawned with the `--subc <connection-file>` argument
- Responsibilities: Connect to the subc daemon over loopback TCP, authenticate using HMAC handshake, and process frames via tokio client loop routed through the per-actor executor

**GitHub CLI routing shim entry point:**
- Location: `crates/aft/src/gh_shim.rs`, `crates/aft/src/main.rs`
- Triggers: Invocation as `gh` (argv[0]) or with `gh-shim` as the first argument (for example, `aft gh-shim`)
- Responsibilities: Validate signed routing manifest, route governed commands via `prefrontal-core` subc `gh.route` seam or exec upstream `gh`, handle `--status` and `--shim-version` self-reports

**Rust binary CLI subcommands:**
- Location: `crates/aft/src/cli/`
- Triggers: `aft warmup` or `aft migrate-storage` invocations
- Responsibilities: Pre-warm tree-sitter grammars, migrate storage between legacy and CortexKit paths

**Release automation entry point:**
- Location: `.github/workflows/release.yml`
- Triggers: Git tag pushes matching `v*`
- Responsibilities: Test the workspace, build platform binaries, publish crates and npm packages, and create a GitHub release

## Error Handling

**Strategy:** Return structured Rust `Response::error` payloads from command handlers, convert failed responses into plugin-side exceptions, and restart hung or crashed worker processes in `packages/aft-bridge/src/bridge.ts`. Under subc mode, mutating panics return an `actor_fatal` error code which triggers a fatal teardown and client teardown across the daemon connection.

## Honest Reporting Convention

**Goal:** an agent reading any AFT response must be able to distinguish three states without ambiguity: (1) the work could not be performed, (2) the work was performed and the result is complete, (3) the work was performed but the result is partial.

**Rule (tri-state):**

1. **`success: false` + `code` + `message`** -- the requested work could not be performed. Codes are machine-actionable strings such as `"path_not_found"`, `"no_lsp_server"`, `"project_too_large"`, `"invalid_request"`, `"ambiguous_match"`. The agent must read the message before continuing.

2. **`success: true` + completion signaling** -- the work was performed. Tools that produce results MUST report whether the result is complete and, if not, name the gaps. Conventional fields:
    - `complete: true` -- the agent can trust absence of items in the result
    - `complete: false` + a named gap field -- partial result. Gap fields include `pending_files`, `unchecked_files`, `scope_warnings`, `skipped_files: [{file, reason}]`, `walk_truncated`
    - `removed: bool` (mutations) -- did the file actually change? `false` is a valid success when the requested change was a no-op.
    - `no_files_matched_scope: bool` (search tools) -- distinguishes "the path/glob you gave me resolved to zero files" from "I searched N files and found nothing"

3. **Side-effect skip codes** -- when the main work succeeded but a non-essential side step was skipped (e.g. post-write formatting), use a `<step>_skipped_reason` field so the agent gets specific feedback without treating the whole call as a failure. Approved values:
    - `format_skipped_reason`: `"unsupported_language"` | `"no_formatter_configured"` | `"formatter_not_installed"` | `"formatter_excluded_path"` | `"timeout"` | `"error"`
    - `validate_skipped_reason`: `"unsupported_language"` | `"no_checker_configured"` | `"checker_not_installed"` | `"timeout"` | `"error"`

**Anti-patterns this convention exists to prevent:**

- Returning `success: true` with empty results when the scope (path/glob) didn't resolve to any files -- the agent reads it as "all clear" but really nothing was checked. Return `no_files_matched_scope: true` (when the scope was syntactically valid but matched zero files) or `success: false, code: "path_not_found"` (when a passed path doesn't exist).
- Reusing one skip-reason string for two distinct causes (e.g., `"not_found"` for both "language has no formatter configured" and "configured formatter binary missing"). The agent has different remediations for each -- split them.
- Silently dropping files that fail to parse / open / decode inside a multi-file or directory operation. Always include a `skipped_files: [{file, reason}]` array so the agent knows X out of Y files were actually processed.
- Asserting `success: true` after a partial transaction without a `complete: false` flag and a list of pending work.

**Where this is documented in code:** `crates/aft/src/protocol.rs` `Response` doc comment carries the canonical rule and the approved field set. New tools must follow this convention; existing tools are migrating.

## Bash Output Compression

**Goal:** reduce hoisted-bash output to fewer tokens while keeping the information the agent actually needs (errors, summaries, ref updates) and discarding the noise (progress bars, repeated headers, deep nested directory listings).

**Five-tier dispatch in `crates/aft/src/compress/mod.rs`:**

1. **Specific Rust `Compressor` modules** -- hand-written parsers for high-traffic tools identified by tool tokens (e.g. `git`, `cargo`, `vitest`). Always wins when matched. Each module lives in its own file under `crates/aft/src/compress/` (e.g. `git.rs`, `cargo.rs`, `eslint.rs`) and implements the `Compressor` trait (`fn tokens(&[&str]) -> bool` + `fn compress(&str, &str) -> String`). Modules include `biome`, `bun`, `cargo`, `eslint`, `git`, `go`, `mypy`, `next`, `npm`, `playwright`, `pnpm`, `prettier`, `pytest`, `ruff`, `tsc`, `vitest`.

2. **Output-shape `Compressor` sniffers** -- inner-tool parsers that recognize their own private summaries even when invoked through wrappers such as `npm test`, `make test`, or `./scripts/check.sh`. Tried after specific modules, before package-manager modules.

3. **Package-manager `Compressor` modules** -- broad head-token matchers (`npm`, `pnpm`, `bun`) that compress unclaimed package-manager output.

4. **Declarative TOML filters** -- strip + truncate + cap + shortcircuit rules for the long tail of CLI tools, loaded from three sources at startup with project > user > builtin priority by filename:
    - **Builtin**: shipped via `include_str!()` from `crates/aft/src/compress/builtin_filters/*.toml`, registered in `crates/aft/src/compress/builtin_filters.rs::ALL`. Currently 22 filters: ansible-playbook, aws, curl, deno, df, docker, du, find, gh, gradle, helm, kubectl, ls, make, pip, psql, terraform, tree, uv, wc, wget, xcodebuild.
    - **User**: `<storage_dir>/filters/*.toml` (XDG-aware via the active `storage_dir`)
    - **Project**: `<project_root>/.cortexkit/aft/filters/*.toml` -- gated by `crate::compress::trust`; never loaded for an untrusted project

5. **Generic fallback** -- ANSI strip + consecutive-line dedup + middle-truncate. Always applies when no Rust module or TOML filter matches.

**Pipeline for TOML filters** (in `crates/aft/src/compress/toml_filter.rs::apply_filter`):

1. ANSI strip (when `[ansi].strip` is true; default true)
2. `[strip]` regexes drop matching lines (multiline mode)
3. `[shortcircuit]` checks remaining content; if matched, return `replacement`. Builtin filters never fabricate non-empty output for empty inputs (empty output stays empty).
4. `[truncate]` middle-truncates per line at `line_max` chars
5. `[cap]` enforces `max_lines` with `keep = "head" | "tail" | "middle"`

**Trust model** (`crates/aft/src/compress/trust.rs`): project filters can lie about output (e.g. strip real failures and replace with `tests: ok`). They are off by default. Users opt in via `npx @cortexkit/aft doctor filters trust`, which records the canonicalized project root in `<storage_dir>/trusted-filter-projects.json` (atomic temp-file rename, deserialized fail-closed). The CLI also exposes `untrust`, `trust --list`, `--show <name>`, and the default list view.

**Concurrency:** the filter registry is exposed as `Arc<RwLock<FilterRegistry>>` so the `BgTaskRegistry` watchdog thread can compress completed task output without holding `AppContext`. The compressor is installed as a closure on `BgTaskRegistry` from `crates/aft/src/main.rs` after `AppContext::new` constructs both.

**Configure invalidation:** `crates/aft/src/commands/configure.rs::handle_configure` calls `ctx.sync_bash_compress_flag()` and `ctx.reset_filter_registry()` on every configure so changes to `experimental.bash.compress`, `storage_dir`, `project_root`, or trust state pick up immediately without restart.

**Compression site:** terminal-state output only. Live tail of running tasks (via `bash_status` polling) is shown raw so agents debugging long commands see exactly what the process emitted. Compression fires inside `BgTaskRegistry::maybe_compress_snapshot` (status / list paths) and `enqueue_completion_locked` (completion frame + `bash_drain_completions` cache).

## Cross-Cutting Concerns

**Logging:** Write plugin logs through `packages/opencode-plugin/src/logger.ts` or `packages/pi-plugin/src/logger.ts` and Rust logs through `env_logger` in `crates/aft/src/main.rs`.

**Caching:** Cache resolved binaries in `~/.cache/aft/bin` through `packages/aft-bridge/src/downloader.ts`, cache session bridges in `packages/aft-bridge/src/pool.ts`, cache tool availability in `crates/aft/src/format.rs`, cache call-graph state in `crates/aft/src/callgraph.rs`, cache trigram search indexes on disk via `crates/aft/src/search_index.rs`, cache semantic embeddings on disk via `crates/aft/src/semantic_index.rs`, cache symbol data on disk via `crates/aft/src/symbol_cache_disk.rs`, and cache read-only borrowed index search/semantic artifacts for external roots/worktrees in a dedicated `borrowed_index_cache` through `crates/aft/src/context.rs` to optimize external search operations.

**Storage:** Store undo snapshots in `crates/aft/src/backup.rs` using the append-only v2 layout (indexing files under `<session_hash>/<path_hash>/` with locks to support multi-session project-shared bridges) governed by configured backup policies (`backup.enabled`, `backup.max_depth`, `backup.max_file_size`). Store named checkpoints in memory in `crates/aft/src/checkpoint.rs`: explicit paths are snapshotted regardless of Git tracking, `file_count` includes only restorable snapshots, skipped paths are reported, and named checkpoints disappear when the bridge or daemon restarts. Store database tables (backups, bash tasks, compression events, state) in `crates/aft/src/db/`, callgraph database in `crates/aft/src/callgraph_store/mod.rs`, inspect diagnostics cache in `crates/aft/src/inspect/cache.rs`, UI metadata on tool output objects, and downloaded binaries in the cache directory managed by `packages/aft-bridge/src/downloader.ts`. Storage lives under the CortexKit shared root (`~/.local/share/cortexkit/aft/`), migrated from the legacy path via `crates/aft/src/migrate_storage.rs`.
