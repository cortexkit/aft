# Configuration

AFT uses a two-level config system: user-level defaults plus project-level overrides.
Both files are JSONC (comments allowed). One location serves every harness:

| Scope | Path |
|---|---|
| User | `~/.config/cortexkit/aft.jsonc` |
| Project | `<project>/.cortexkit/aft.jsonc` |

For the removal order and harness-specific registration steps, see [Uninstall](../README.md#uninstall).

OMP uses this same CortexKit user file; register its Pi-compatible plugin with `npx @cortexkit/aft@latest setup --harness omp`.

`bash.watch_sync_max_ms` bounds synchronous `bash_watch` calls in a main session, which should only cover a short remaining wait on a task; it defaults to 120 seconds because longer synchronous waits keep the agent turn occupied. For longer commands, use `bash({background:true})` and let the completion reminder wake you, or use `bash({wait:true})` when the result is needed before anything else. A delegated (subagent) session is not bounded by it; it uses `bash.worker_wait_max_ms` instead. Values are clamped to 1000..=1800000 with a warning; set it to `1800000` in user or project config to restore the old 30-minute cap.

`bash.worker_wait_max_ms` bounds blocking `bash` calls in every session, including heads (`wait: true`, or foreground calls forced to block, such as when `bash.subagent_background` is false). It also bounds delegated (subagent) `bash_watch` calls, including an explicit timeout. Head watches still use `bash.watch_sync_max_ms`. At the limit the command is not killed: it keeps running in the background, and the caller gets its task id, elapsed time and latest output, so it can inspect it, wait again where `bash_watch` is available, or `bash_kill` it. Blocking calls and worker watches push the command's default 30-minute hard kill to at least this long after the wait; an explicit command `timeout` is never extended. The default is `1800000` (30 minutes); a value below `60000` is a config error that drops the `bash` block, not a silent clamp. User and project config may both set it, and harness blocks override it like the other `bash` keys. The key was originally worker-only; its name is retained so existing user and project configurations keep working when blocking head calls use it too.

`bash.changed_files` lists the project files that changed while a shell command ran. Only user configuration can turn it on; a project can only set `"off"`.

- `"off"` (default): nothing is recorded.
- `"list"`: each changed path, marked added, modified or deleted.
- `"diff"`: the list plus a short diff for clean tracked text files, against the version git had when the command started.

The list covers a time window, so changes made by other processes in that window can appear. It comes from the file watcher, so paths the watcher excludes (`.git`, `target`, `node_modules`, ignore rules) are not listed. Before reporting, AFT confirms the watcher has caught up by creating and removing an empty marker file (in `.git/`, or the project root when `.git` is not a folder), waiting at most 300 ms; if it can't confirm, the result says the list may be incomplete. Background commands report in their completion notice, and `runon` commands report only changes seen in the local project. Limits: 40 files and 64 KiB per report, 200 lines and 32 KiB per file diff. Files over 256 KiB, binary files and files that already had uncommitted changes are listed without a diff.

`bash.env` adds fixed variables to every shell command from user config only (for example, `"env": { "TZ": "UTC" }`). Project config cannot set environment values. Per-call `env` entries override these defaults, and user-config changes apply to future commands without a restart. Loader, shell/interpreter startup, `PATH`, and other protected variables are rejected while loading config. The first bash reply in each session lists the configured keys only, never their values. When a command starts while the same trimmed command is already running in the same project and canonical working directory, AFT adds a warning with its task ID, age, and whether it belongs to this session; the command still runs.

On Linux, user config may set `bash.linux_scope: true` to launch non-PTY tool shells through `systemd-run --user --scope --collect --quiet`. The default is `false`. AFT uses the scope only when `systemd-run` exists and the user manager is reachable; otherwise it falls back to the normal process-group-isolated spawn and writes one informational log line. Native-sandbox launches also use the normal spawn because their launcher cannot contact the user manager. Project config cannot enable or disable this host-level containment option.

`bash.memory_limit_mib` optionally caps aggregate memory for each local command, including foreground, background, and PTY commands. It is off by default; set a positive integer in MiB (for example, `4096` for 4 GiB; maximum `4294967295`) in user config. Projects may tighten the cap, never raise or remove it. Changes affect future commands; there is no per-call override. Remote `runon` jobs use the build server's own limits and are not covered.

- **Linux:** cgroup v2 `memory.max` (using a delegated transient systemd user scope when needed); if unavailable, the result explicitly names the process-group RSS watcher fallback.
- **Windows:** a job-object aggregate memory limit refuses allocations; AFT stops the job when a limit notification or peak accounting confirms the cap was hit.
- **macOS:** a physical-footprint process-group watcher samples every 500 ms. Fast allocations can overshoot for a fraction of a second, and processes that leave the group are not counted. The Linux watcher has the same limits. Watcher scans are bounded to 4096 processes and stop the command explicitly if accounting cannot complete.

On macOS, `bash.disclaim_privacy: true` makes agent shells (including background, PTY, and governed `gh` commands) responsible for their own privacy permissions instead of inheriting the supervisor's grants. It defaults to `false`; user config controls it and project config may only turn it on. It is live-reloadable for future commands, not already-running children, and accepted but inert on other platforms. Disclaimed shells cannot rely on inherited access to protected folders such as `~/Downloads`; agents should use AFT's in-process `read` tool for those files (`grep` and `glob` are also unaffected). If macOS cannot apply the responsibility attribute, AFT refuses the command with `privacy disclaim unavailable` rather than running it with inherited grants. Effective per-root settings appear in health, and the first disclaimed command logs an informational message per session.

Background-task completion and pattern-watch notices are delivered only to the session that started the task; other sessions bound to the same project may still inspect or stop the task by ID.

Older installs used per-harness paths (`~/.config/opencode/aft.jsonc`, `~/.pi/agent/aft.jsonc`,
and their project-level equivalents). On first load, the plugin migrates them to the CortexKit
location automatically and leaves a `.MOVED_READPLEASE` marker behind.

## Harness-specific overrides

Use the top-level `harnesses` object when the same machine runs more than one AFT host. Each
entry may set any normal config field except `harnesses`; nested `harnesses` objects are ignored
with a warning. Unknown harness names are ignored so a shared config remains forward-compatible.

For the active harness, AFT resolves settings in this exact order:

1. base user config
2. user `harnesses.<active>` override
3. base project config
4. project `harnesses.<active>` override

An override wins within its own tier. The existing project trust boundary is applied only after
its harness override is combined: project harness overrides can change project-safe fields such
as `edit_mode`, but cannot supply user-only settings such as LSP executable configuration,
semantic credentials, subc transport, or sandbox weakening.

For example, keep every tool on OpenCode while Pi keeps its native `grep`
(a harness block can only add disables to the base list; removing a base
disable requires editing the base list):

```jsonc
{
  "disabled_tools": [],
  "harnesses": {
    "pi": {
      "disabled_tools": ["grep"]
    }
  }
}
```

## Storage Root Environment Override

On Unix, AFT storage is owner-only (0700 directories and executables, 0600 files), and existing loose storage directories are tightened when opened without walking their contents.

For a missing checkout folder, index retention uses a one-day age window from the last bind when the persisted binding verifies a linked Git worktree, and seven days for main checkouts or older/unknown bindings; reader, pin, lease and mount protections still apply.

Set `AFT_STORAGE_DIR` to place AFT's SQLite databases, WALs, writer leases, and indexes on a local disk when `$HOME` is NFS-mounted (for example on corporate or HPC systems). The variable is process state, not a JSONC configuration key, and an empty value is treated as unset. Relative values are resolved to an absolute path at first read; `~` and `~/...` are expanded using the current user's home directory.

Storage resolution is identical for plugins, standalone binaries, and warmup:
`AFT_STORAGE_DIR` (explicit override) > `XDG_DATA_HOME/cortexkit/aft` when set > the platform data directory (`~/.local/share/cortexkit/aft` on POSIX, or `%LOCALAPPDATA%/cortexkit/aft` on Windows with its documented fallbacks). The statfs-based root key refuses to combine storage roots from different filesystems, preventing a local override from silently sharing indexes with the old NFS root.

AFT caches the login-shell PATH probe in `<storage_dir>/effective-path.json`. The cache records the shell startup files and is invalidated when one is created, removed, or changes size or modification time. A cached timeout stores a null PATH, so a blocked shell profile delays only the first launch after its startup files change; AFT refreshes the cache in a detached helper for a later launch.

### Rebuildable cache root (`storage.cache_dir`)

Set this **at the top level of the user config only** to keep large, rebuildable artifacts on a separate local volume, for example a no-backup APFS volume:

```jsonc
{
  "storage": { "cache_dir": "/Volumes/AFT-Caches" }
}
```

The path must already exist and be absolute after home-directory expansion (`~` alone or a path beginning with `~/` or `~\`). AFT never creates this directory. It creates only its owner-only `aft/` namespace inside it (0700 directories and 0600 files on Unix). Existing namespaces must have an ownership marker whose random registration ID and directory identity match this storage root's durable registration; copying a marker into an unrelated directory cannot enroll it. Cache locations cannot overlap the durable storage root or an artifact namespace belonging to any configured storage root. Project values and **all harness overrides, including user harness overrides**, are dropped with a dropped-key notice: one storage root has one cache location, so borrowers on different harnesses see the same owner artifacts. A change takes effect on configure/reconnect, not a live config reload.

A path that runs through symlinks (macOS `/var` or `/tmp`, a symlinked home, an alias of a `/Volumes` mount) is resolved once when the directory is first used and pinned by device and inode; if the configured spelling later names a different directory, caches turn off as below. The same applies to a storage root reached through a symlink. Inside AFT's own folders symlinks are never followed. A missing directory, non-directory, failed write probe, changed remembered directory identity, or unusable namespace emits `cache_dir_unavailable`: `storage.cache_dir <path> is unavailable (<reason>); AFT is running without its caches until it is back.` There is **no fallback to the storage root**. Trigram, semantic, callgraph, views/blobs, symbol-disk and inspect lanes do not open or build disk caches. Search still returns its bounded fallback walk with an honest partial trailer; semantic/callgraph and health report the typed unavailable reason. Every configure re-checks the directory. The background index coordinator probes immediately on its first check and then at most once a minute for each storage/cache-directory identity pair. It verifies each context's ownership before resuming caches through configure; the warning/reason clears when usable. An unreadable user config, one that does not parse, or one whose `storage` section or any `harnesses.<name>.storage` override is invalid emits `cache_dir_config_unknown` (the reason names the failing section) and keeps caches off rather than assuming the setting was unset; repair the user config and reconnect to revalidate its cache location. An invalid value in any other section is reported by normal config validation and leaves caches on.

After first successful use, AFT remembers the configured directory's device/inode identity (Windows volume serial/file id) and volume UUID when available in durable `retention/cache-parents.json`, not on the cache volume. A mismatch reports `cache_dir is not the volume it was` and keeps caches off; selecting a new path starts fresh. On macOS, `/Volumes` itself is refused, and a path below `/Volumes/<name>` on the system or Data volume is treated as an unmounted-volume stub, even on first use. First enrollment requires a cache parent on a different device from the durable storage root (or its nearest existing ancestor). A same-device directory, including an unmounted mount-point stub outside `/Volumes`, is refused with `cache_dir_on_storage_device`, visible in configure warnings, health/memory census and doctor. A previously registered parent still has to pass the saved-identity check on every bind.

The artifact root, `<storage.cache_dir>/aft`, contains `views/`, `blobs/`, `callgraph/`, `semantic/` (including local embedding and reranker models), `index/`, `symbols/` and `inspect/`. Views and blobs always share this root and volume. Writer leases, reader markers, publication temporaries and retention trash stay beside their cache artifacts, so moving caches does not introduce cross-volume publish or retirement renames. Health, memory census and `aft doctor` report the cache location. `AFT_CACHE_DIR` still controls downloaded binaries and language-server packages; it is not this setting.

Leaving the setting unset preserves the existing layout for a storage root that has never used a cache directory. Once a storage root has kept its caches in a cache directory, removing the setting (or losing it to a typo or a missing user config) keeps caches off with `cache_dir_unavailable`: `storage.cache_dir is not set, but this storage root's caches live on <parent>; set it again, or set "cache_dir": null to move caches back to the primary disk.` Set `"storage": { "cache_dir": null }` to deliberately return caches to the storage root. Storage sections and storage-like misspellings (such as `storge` or `cachedir`) are `invalid_cache_config` errors, not defaults. Unknown sections whose values contain `cache_dir` and section names within two single-character edits of `storage` are treated as storage-like; unrelated unknown keys retain the existing warning/partial-load behavior. The distinct process-state name `storage_dir` is not interpreted as a cache-parent selector. Durable state stays in the storage root selected above: `aft.db` and its WAL/SHM, checkpoints, harness backups and bash tasks, shims, retention metadata, artifact-owner manifests, dSYMs, logs, `engram-catalog.json` and `trusted-filter-projects.json`. ENGRAM's catalog remains relative to that storage root and declares only folders that actually exist there; caches on another volume are not catalog entries.

Changing the setting builds fresh caches; nothing is migrated. Stop AFT sessions using the storage root being cleaned. Updated cache-using processes, including standalone sessions and `aft index`, hold a shared kernel lock at `<storage_dir>/retention/daemon-use.lock` before cache access; the main process retains it until the main process exits. Configuration snapshots hold a lease for any additional storage root they select. Cleanup takes the same lock exclusively before preparing cache namespaces and retains it across inventory, confirmation and deletion. For this release's upgrade window, a conservative host-wide subc-daemon check additionally refuses cleanup while a daemon is running, including older daemon versions that do not hold the root lock. Stop those daemons before cleanup. `aft doctor --fix` reclaims only `views/`, `blobs/`, `callgraph/`, `semantic/`, `index/`, `symbols/` and `inspect/` inside recorded, marked, inactive artifact roots. It never removes unmarked folders under the durable storage root.

For the original storage-root caches, run the separate `aft doctor --clean-old-caches` operation (native `aft clean-old-caches`). This is an explicit, confirmation-gated exception for old unmarked caches: **only** `views/`, `blobs/`, `callgraph/`, `semantic/`, `index/`, `symbols/` and `inspect/` directly under the resolved durable storage root are eligible. The root must already contain a regular `aft.db` or its matching AFT owner marker; cleanup cannot create its own ownership proof. It must not be the active artifact root, and `storage.cache_dir` must be active, enrolled and on a different device. Harness folders, other names and durable state are never selected. Symlinked or foreign-device domains, nested mounts and hardlinked files refuse the operation. The prompt lists every exact candidate path and byte size plus the total, then requires the exact word `delete`; without a terminal, explicit `--yes` is required. See `aft doctor --clean-old-caches --help` for this exception and its prerequisites.

Both operations preflight every root and candidate before deleting anything, refuse live daemons independently of cache leases, and delete through pinned directory handles. Cache directory/file creation also walks from the admitted root handle, refusing symlinks and foreign mounts below it; a pathname is not reused as creation authority after that walk. Root/candidate symlinks, changed identities, nested mounts and aliases of durable or active directories refuse cleanup. Preflight is read-only and refuses symlinks, special files and linked coordination files anywhere it inspects; marker inspection and any marker sweeping use bounded, no-follow, directory-handle-relative operations. Existing coordination files also refuse cleanup rather than being reclaimed or heartbeated: stop their owner and repair stale locks before retrying. Each context holds `.aft-in-use.lock` shared while using a root, including retired roots with in-flight readers; cache/semantic/symbol/writer locks and pins provide additional protection. Unavailable configured roots refuse cleanup altogether. Partial failures report completed removals and partial-directory progress. On Windows filesystems without POSIX delete disposition, an accepted legacy disposition counts as removed with a `removed_with_warning` entry explaining that removal remains pending until handles close. Root directories and durable state are never removed; ordinary startup and plain `aft doctor` do not delete caches.

Background retirement records the validated source and top-level member identities in `.aft-retirement.json` before moving data into domain- or mount-local trash. The reaper checks that proof on every retry and keeps it until the payload has been removed, so interrupted unlinks can resume without trusting a replacement directory. Unverified or pre-existing trash without a matching proof is retained rather than guessed to be safe. Mount-local pending hints remain on durable storage and are cleared only after the matching trash generation and its directory barriers have completed.

Protection censuses, size inventories and recursive removal each have a 64-level depth limit, a one-million-entry budget and a 60-second traversal deadline. An incomplete census or inventory refuses deletion, including when durable state contains a nested mount whose identities cannot safely be enumerated. A removal that exceeds its budget after preflight stops and reports any partial progress; inspect the oversized tree before retrying.

## Uninstall paths

Delete the user and project config files listed above, then delete the data roots below. A non-empty environment override takes precedence over the corresponding default.

**Shared storage root** (`AFT_STORAGE_DIR`): if unset, AFT uses `XDG_DATA_HOME/cortexkit/aft/` when `XDG_DATA_HOME` is set. Otherwise, POSIX uses `~/.local/share/cortexkit/aft/`; Windows uses `%LOCALAPPDATA%/cortexkit/aft/`, then `%APPDATA%/cortexkit/aft/`, then `%USERPROFILE%/AppData/Local/cortexkit/aft/`. This root contains indexes, databases, background-task records, logs, and backup history.

**Rebuildable artifact namespaces** (`storage.cache_dir`): if configured, also remove the registered `<storage.cache_dir>/aft/` namespace and any inactive namespaces recorded in the storage root. Do not delete the cache parent itself: it may contain unrelated data.

**Downloaded-binary and LSP cache** (`AFT_CACHE_DIR`): if unset, POSIX uses `${XDG_CACHE_HOME}/aft/` when `XDG_CACHE_HOME` is set, otherwise `~/.cache/aft/`. Windows uses `%LOCALAPPDATA%/aft/`, then `%APPDATA%/aft/`, then `%USERPROFILE%/AppData/Local/aft/`. The `bin/`, `lsp-packages/`, and `lsp-binaries/` subdirectories are under this root.

The backup store treats its on-disk tree as authoritative across processes; deleting the storage root permanently deletes undo history for past edits, but does not delete project files.

## CPU profile

On macOS, profile a running AFT subc daemon with its matching release dSYM in one command:

```sh
npx @cortexkit/aft doctor --profile 4
```

`--profile` accepts an optional sampling duration in seconds. The command finds a single
`aft --subc` or `ck-aft --subc` process (or use native `aft profile --pid <pid>`), verifies the
running image UUID against a local or downloaded dSYM, and reports a running-versus-waiting
thread census. Pass `--json` through to the native command for tooling.

```text
AFT CPU profile (macos-sample)
pid: 48123
Thread census (running / total):
  48124 search-worker: 392 / 400 running (8 waiting) — search_index
Top inclusive running symbols:
    392 aft::search_index::build ...
```

Raw sampler output is withheld unless native `aft profile --raw` is explicitly requested.

## Config Options

```jsonc
{
   // Edit/read surface: "default" (default) or "hashline". User and project
   // tiers both accept this key; ordinary project-over-user precedence applies.
   "edit_mode": "default",

   // Optional existing rebuildable-cache parent, user base only. Omit to keep
   // the default storage-root layout. See "Rebuildable cache root" above.
   // "storage": { "cache_dir": "/existing/cache-parent" },

  // Auto-format files after edits. Default: false. When enabled, formatting is
  // queued and runs after ~90s without further edits to the file.
  "format_on_edit": false,

  // Let read, aft_outline and aft_zoom take a `ref` argument that reads a file
  // as it is at a git revision. Default: false. User and project tiers.
  "revision_reads": false,

  // Auto-validate after edits: "syntax" (tree-sitter, fast) or "full" (runs type checker)
  "validate_on_edit": "syntax",

  // Per-language formatter overrides (auto-detected from project config files if omitted)
  // Keys: "typescript", "python", "rust", "go"
  // Values: "biome" | "oxfmt" | "prettier" | "deno" | "ruff" | "black" | "rustfmt" | "goimports" | "gofmt" | "none"
  "formatter": {
    "typescript": "biome",
    "rust": "rustfmt"
  },

  // Per-language type checker overrides (auto-detected if omitted)
  // Keys: "typescript", "python", "rust", "go"
  // Values: "tsc" | "tsgo" | "biome" | "pyright" | "ruff" | "cargo" | "go" | "staticcheck" | "none"
  "checker": {
    "typescript": "biome"
  },

  // How missing formatter/checker/LSP warnings appear after configure.
  // Default: "toast" — 10s TUI/HTTP toast, no session chat pollution.
  // "log" — plugin log only. "chat" — legacy ignored messages in the transcript.
  // Formatter warnings run only when format_on_edit is true or formatter.<lang> is set.
  // Checker warnings run only when validate_on_edit is "syntax"/"full" or checker.<lang> is set.
  // (There is no top-level "formatters" key — use format_on_edit / formatter / checker.)
  "configure_warnings_delivery": "toast",

  // Tools that are NOT registered. Every other AFT tool is registered:
  // read, write, edit, apply_patch, grep, glob, bash (plus the independent
  // bash_status/bash_watch/bash_write/bash_kill companions), aft_outline,
  // aft_zoom, aft_search, aft_callgraph, aft_inspect, aft_import, aft_safety,
  // aft_conflicts, aft_delete, aft_move, ast_grep_search, ast_grep_replace.
  // Absent from the user config => ["aft_move", "aft_delete"]. An explicit list
  // replaces that default: [] enables both, ["aft_search"] disables only search.
  // Disabling a host name (read, grep, bash, ...) leaves the host's own tool.
  // Harness blocks and project config can only add names; a project cannot
  // disable aft_safety or a host tool slot. Unknown names are kept and
  // reported once per load. Pi/OMP have no apply_patch or glob tools.
  "disabled_tools": ["aft_move", "aft_delete"],

  // Background indexes. Each defaults on and builds independently of which
  // tools are registered. A project config can switch an index off but never
  // back on. Consumers (grep/glob, aft_search, aft_callgraph, ...) report an
  // off or building index instead of disappearing.
  "indexes": {
    // Trigram index for indexed grep/glob and the lexical aft_search lane.
    "trigram": true,
    // Semantic index for the semantic aft_search lane. The local default backend
    // may download an ONNX runtime and model and use CPU.
    "semantic": true,
    // Persisted call-graph store for aft_callgraph and enrichment.
    "callgraph": true
  },

  // Borrow-only checkout reconciliation and RAM overlay. Default: true.
  // A linked worktree, or a clone or copy of a repository whose live checkout
  // owns the shared indexes, borrows that checkout's search index read-only.
  // With this on, the borrowed index is compared with this checkout's own files
  // before search reports ready, and this checkout's file-watcher events go to
  // private in-RAM search and semantic deltas (and invalidate the symbol cache)
  // so search sees local edits. Semantic embeddings are created only for files
  // changed after bind; corpus catch-up remains disabled. RAM cost scales with
  // the number of files that differ. The shared indexes are never written, and
  // the callgraph stays frozen. Setting it false serves the borrowed index as
  // it is, so glob and search can list the live checkout's files instead of
  // this one's; it is kept only as an escape hatch. User and project tiers may
  // both set this.
  "worktree": {
    "ram_overlay": true
  },

  // Content-addressed index views. When enabled, semantic and callgraph artifacts
  // are assembled from reusable per-file blobs behind an atomic manifest.
  // User and project tiers may both set this. Default: false.
  "views": {
    "enabled": false
  },

  // When project_root is exactly $HOME, every index is force-disabled because the
  // home directory is not treated as a project root.

  // Optional embedding-backend configuration for aft_search. Omit this block to use
  // the local fastembed default. Three backends are supported: "fastembed" (default,
  // local ONNX), "openai_compatible" (any /v1/embeddings endpoint — OpenAI, Together,
  // Voyage, vLLM, LM Studio, etc.), and "ollama" (self-hosted at /api/embeddings).
  //
  // USER-only fields: "backend", "base_url", "api_key_env" (project config cannot
  // inject these — strict-allowlist trust boundary). Project config can tune
  // "model", "query_instruction", "timeout_ms", "max_batch_size", "max_files",
  // and "max_input_tokens".
  //
  // Switching "backend", "model", or "base_url" deletes the persisted index and
  // rebuilds from scratch on next session start (necessary because dimensions and
  // semantic spaces differ across models). Rotating an API key without changing
  // "api_key_env" does NOT trigger a rebuild.
  "semantic": {
    "backend": "fastembed",            // "fastembed" | "openai_compatible" | "ollama"
    "model": "all-MiniLM-L6-v2",       // model id understood by the backend
    "query_instruction": "auto",        // QUERY-only: "auto" (default), "off", or literal task text.
                                         // Auto applies Qwen3-Embedding's model-card retrieval task;
                                         // fastembed and other model families remain bare. This does not
                                         // change document vectors or trigger an index rebuild.
    // "base_url": "https://api.openai.com/v1",   // required for openai_compatible / ollama
    // "api_key_env": "OPENAI_API_KEY",            // env var name (not the key itself)
    "timeout_ms": 25000,                // INDEX BUILD request floor. HTTP batch deadlines scale with
                                         // batch size and a successful wall-time-per-item EMA (25% new,
                                         // 75% prior) with 2x safety. Before the first success, the
                                         // deadline is max(timeout_ms, timeout_ms * batch_size / 16).
                                         // A timed-out batch halves immediately; only a one-item timeout
                                         // at this base floor is treated as backend-down evidence.
    "query_timeout_ms": 3000,           // per-request timeout for interactive QUERY embeds (500-30000).
                                        // Raise for slow providers; on timeout, search degrades to
                                        // lexical for that query instead of failing.
    "max_batch_size": 64,               // maximum adaptive build batch size. Timeout halves the active
                                         // size; two successes at that size allow doubling toward this max.
                                         // With the default max 64 and no successful latency sample, a
                                         // never-answering backend reaches the one-item verdict within 11
                                         // timeout_ms floors (275 s), plus scheduler overhead.
    "max_files": 20000,                 // max files indexed (default 20000); raise for remote backends
    // "max_input_tokens": 512          // advanced: per-row token budget for remote backends; widens the
                                        // symbol-body slice each chunk embeds (default keeps the 512-safe
                                        // caps; fastembed ignores it). Changing it rebuilds the index.
                                        // Measured 2026-09: longer bodies did not improve retrieval.
  },

  // Restrict all file operations to the project root directory.
  // Default: false. Matches OpenCode's and Pi's native behavior — neither host
  // hard-rejects out-of-root paths from their built-in tools (OpenCode prompts
  // the user; Pi just allows). Set to true to enforce a strict project-root
  // boundary on every AFT tool call. USER-only — strict-allowlist trust
  // boundary refuses to honor this field from project-level config so a
  // hostile repository cannot weaken your file boundary.
  "restrict_to_project_root": false,

  "pi": {
    // Plain Pi: ask in Pi's own terminal UI before an AFT tool reaches a path
    // outside the project. Default: true. Off, outside access is allowed
    // without asking. Print, JSON and RPC sessions have no screen to ask on
    // and never ask. Credentials, AFT's data folder, `sandbox.read_deny` and
    // `restrict_to_project_root: true` refuse either way. Only user config can
    // turn it off; a project config may only turn it on. Not used on OMP,
    // where OMP's own `tools.approvalMode` decides: AFT reports a call that
    // names an outside path as the "exec" tier there.
    "external_access_ask": true
  },

  // OpenCode plugin only. When true, the auto-update hook installs newer
  // @cortexkit/aft-opencode versions automatically when your OpenCode plugin
  // entry is unpinned (no version, or a `latest` tag). When false, the hook still
  // notifies you that an update is available but does not install it. Local-dev
  // (file://) and pinned (@x.y.z) installs always notify-only regardless of this
  // setting. `aft setup` and `aft doctor --fix` write a pinned entry matching the
  // CLI version, so update by running `npx @cortexkit/aft@latest doctor --fix`.
  // Default: true. USER-only — strict-allowlist trust boundary refuses to honor
  // this field from project-level config to prevent hostile repos from silently
  // suppressing security updates.
  "auto_update": true,

  //   typescript-language-server, pyright-langserver, rust-analyzer, gopls,
  //   bash-language-server, yaml-language-server
  //
  // Add your own with `lsp.servers`. Disable any with `lsp.disabled`.
  "lsp": {
    "servers": {
      "tinymist": {
        "extensions": [".typ"],
        "binary": "tinymist",
        "args": [],
        "root_markers": [".git", "typst.toml"],
        "env": {                  // optional — extra env vars passed to the spawned server
          "TYPST_FONT_PATHS": "/usr/share/fonts"
        },
        "initialization_options": {  // optional — server-specific LSP `initializationOptions`
          "formatterMode": "typstyle"
        }
      }
    },
    // Disable any registered server by id. IDs are case-insensitive. Built-in
    // ids: typescript, python, rust, go, bash, yaml, ty. Custom servers use
    // the key under `lsp.servers` (e.g. `tinymist`).
    "disabled": ["python"],
    "python": "ty",  // "auto" (default) | "pyright" | "ty"

    // LRU cap for the in-memory diagnostic cache.
    // Bigger = more files retained across the session.
    // Default: 5000. Set to 0 to disable cap (live dangerously on huge monorepos).
    "diagnostic_cache_size": 5000
  },

  // Bash runtime configuration (graduated from experimental.bash.* in v0.27.2).
  // Registration of `bash` and its companions is controlled by disabled_tools;
  // `bash: false` or `bash.enabled: false` turns the runtime gate off, and
  // every bash operation (including the companions) then reports bash_disabled.
  "bash": {
    // Runtime gate. Default true.
    "enabled": true,

    // Fixed extra variables for every shell command. User-tier only; project
    // config cannot set environment values. Per-call env overrides these.
    // Protected loader, shell/interpreter startup and PATH variables are refused.
    "env": { "TZ": "UTC" },

    // Rewrite common shell commands (cat / grep / find / sed / ls / rg / cat >>)
    // to AFT tools. Adds a footer hint nudging the agent to call the AFT tool
    // directly next time. Default false.
    "rewrite": false,

    // Compress bash output via the five-tier compressor pipeline (specific Rust
    // compressors → output-shape sniffers → package-manager compressors → TOML
    // filters → generic ANSI-strip + dedup). Pass `compressed: false` on a single
    // bash call to opt out for that call. Default false.
    "compress": false,

    // Enable background bash via `bash({ background: true })` and PTY via
    // `bash({ pty: true })`. Completed-but-unread tasks surface on the next
    // foreground tool call as `bg_completions` and via an automatic reminder.
    // Default false.
    "background": false,

    // Allow subagents to run background bash. When false, `background: true`
    // is converted to a foreground call that blocks until the command
    // finishes, at most `worker_wait_max_ms` (after which the command moves to
    // the background and the subagent is told how to keep waiting), and an
    // async `bash_watch` becomes a sync wait of up to `worker_wait_max_ms`.
    // Default true because workers are
    // multi-turn and use bash_watch to wait. OpenCode applies it to sessions
    // with a parent session; Pi applies it to headless runs (`pi -p`,
    // `--mode json`) and to processes started with MAGIC_CONTEXT_PI_SUBAGENT=1,
    // where `pty: true` is then refused.
    "subagent_background": true,

    // How long a foreground bash call blocks before auto-promoting the task
    // to the background. Minimum 5000; lower values are clamped up. Default 8000.
    "foreground_wait_window_ms": 8000,

    // Pi-only fallback for older Pi versions that cannot report whether its
    // optional default PowerShell tool is enabled. OpenCode never registers it.
    "powershell_tool": false,

    // Whether any new message (typed, or one such as a subagent's completion)
    // detaches a blocking `wait: true` bash call to the background and ends a
    // waiting `bash_watch`. Default true. Set false to keep both blocking
    // through such messages; even then, a message containing `&detach` forces the
    // detach (the token is stripped before the model sees the message).
    "detach_on_user_message": true,
    // Read-only schema trailers after missing-table/column errors. Default true;
    // user and project tiers. SQLite on Unix; literal paths only (no variables,
    // globs or memory databases; file: URIs require mode=ro, and startup -cmd/
    // -init flags are skipped because they may change the connection).
    // Finished commands only, including
    // background completions, independent of compression. Probe: same launch
    // sandbox, 1.5s timeout, 256 KiB output cap; trailer: 2 KiB with omissions.
    "db_schema_hints": true,
    "changed_files": "off",

    // Maximum time a synchronous bash_watch call may wait. Defaults to 120000ms;
    // values outside 1000..=1800000 are clamped with a warning. Sync waits are
    // intended for a short remaining wait; to restore the old 30-minute cap,
    // set this to 1800000 in the user or project config.
    "watch_sync_max_ms": 120000,

    // How long a delegated (subagent) session waits on one command before
    // control returns to it (a bash_watch without a timeout, or a blocking
    // bash call). The command keeps running in the background. Default
    // 1800000 (30 minutes); values below 60000 are a config error.
    "worker_wait_max_ms": 1800000,

    // Linux-only and user-tier only. Put non-PTY tool shells in transient
    // systemd user scopes when the user manager is reachable. Default false.
    "linux_scope": false
  },

  // aft_inspect codebase-health scanner (recommended/all tiers).
  "inspect": {
    "enabled": true,              // runtime switch; false makes aft_inspect report inspect_disabled
    // Blocking LSP diagnostics deadline. Default 120000; values clamp to
    // 10000..600000. User config sets the baseline; project config may raise
    // it but cannot lower it, so a repository cannot silently reduce another
    // consumer's diagnostic completeness.
    "diagnostics_timeout_ms": 120000,
    "tier2_idle_minutes": 5,      // debounce before idle-triggered Tier 2 background scans
    // Computation switches, all true by default. Projects can turn categories
    // off, never restore a category the user turned off. Applies live.
    "categories": {
      "diagnostics": true, "todos": true, "dead_code": true,
      "unused_exports": true, "duplicates": true, "cycles": true,
      "complexity": true
    },
    "duplicates": {
      // Intentional mirror pairs, matched against project-root-relative
      // forward-slash paths. Groups fully spanning one pair are suppressed but
      // still counted in the duplicates summary.
      "expected_mirrors": [["plugin/**", "pi-plugin/**"]]
    }
  },

  // Idle reclamation. User and project tiers. Values outside the documented
  // ranges are clamped with a warning; non-integers are dropped with a warning.
  // Reclaimed indexes rebuild on the next request.
  "idle": {
    // Minutes without tool traffic before an unbound root's artifacts are
    // evicted. Default 30; clamped to 5..=30.
    "root_ttl_minutes": 30
  },

  // Automatic undo snapshots. Existing-file mutations larger than 64 MiB and
  // every mutation under an OS temporary directory proceed without an undo
  // snapshot and report why undo is unavailable.
  "backup": {
    // User-only master switch and per-file history depth.
    "enabled": true,
    "max_depth": 20,
    // Maximum existing-file size captured for undo, in bytes. Default 64 MiB.
    // User and project tiers may set this value; project config wins. Explicit
    // larger values are honored. Set 0 to disable automatic snapshots.
    "max_file_size": 67108864
  },

  // Shell profiles by role; untrusted routes always use strict.
  "sandbox": {
    "profile": { "worker": "relaxed", "head": "off" },
    // Additional strict-profile writable roots. User config only.
    "write_allow": [],
    // Additional paths to hide from sandboxed commands.
    "read_deny": []
  },

  // Remote runs requested per bash call with `runon` (subc mode). Default: off.
  "remote_exec": {
    // User config only; a project config may set false to turn them off.
    "enabled": false,
    // The runner demand a `runon` call without specifics runs under. User config only.
    "default_demand": "linux"
  },

  "experimental": {
    // Use the experimental Astral `ty` Python type checker.
    // Implied when `lsp.python === "ty"`.
    "lsp_ty": false
  },

  // User-only GitHub integration gates.
  "github": {
    "shim": true,    // Interpose the governed gh shim in agent child PATHs.
    "read": false,   // Enable issue:// and pr:// reads, outlines, and zooms.
    "write": false   // Create and edit conversation comments (implies read).
  },


  // Git co-authorship for commits made by AFT-spawned agent children.
  // "off" (default) | "auto" | an explicit "Name <email>" identity.
  // User and project tiers are accepted; normal project-over-user precedence applies.
    "git": {
      "co_author": "off"
    }
  }
```

On Pi versions that expose the live default-tool registry, AFT registers `powershell` only when Pi has enabled its optional built-in tool. If that registry is unavailable, set `bash.powershell_tool` to `true` to mirror Pi's setting explicitly. The default is `false`; this key does not register a tool on OpenCode.

AFT auto-detects the formatter and checker from project config files (`biome.json` → biome,
`.oxfmtrc.json` / `.oxfmtrc.jsonc` / `oxfmt.config.ts` → oxfmt, `.prettierrc` → prettier,
`Cargo.toml` → rustfmt, `pyproject.toml` → ruff/black, `go.mod` → goimports). Local tool binaries
(biome, oxfmt, prettier, tsc, pyright) are discovered in
`node_modules/.bin` before falling back to the system PATH. You only need per-language overrides
if auto-detection picks the wrong tool or you want to pin a specific formatter.

### Hashline edit mode

Set `edit_mode` to `"hashline"` to make `edit` accept exactly `{ "patch": "..." }` and to render text reads with snapshot tags used by those patches. Other tools, including `write` and `apply_patch`, keep their existing schemas and behavior. The setting defaults to `"default"`; it is accepted in both user and project config, with ordinary project-over-user precedence. An unknown value emits a configure warning and falls back to `"default"`.

See the [Hashline patch grammar](hashline.md) for section headers, addresses, operations, and tag freshness rules.

A hashline mutation attempts to register every affected path before changing files. An actual backup error still fails the edit before mutation. Policy skips for an oversized file or an OS temporary path allow the edit to proceed, and the response states that undo is unavailable for that change.

Hashline mode needs both the `read` and `edit` tools. If `disabled_tools` removes either, AFT keeps the ordinary edit/read behavior for the registered tools and emits exactly one configure-time warning per load: `hashline_read_disabled` when `read` is disabled (it takes precedence), otherwise `hashline_edit_disabled`. No other tool is unregistered.

## Search reranking

`search.rerank` lets a cross-encoder reorder the head of `aft_search` results. It is off by default.

```jsonc
{
  "search": {
    "rerank": {
      "backend": "off",          // "off" (default) | "onnx" | "remote" | "synapse"
      // "model": "...",           // onnx: bge-reranker-base (default), bge-reranker-v2-m3,
                                  //   jina-reranker-v1-turbo or gte-reranker-modernbert-base.
                                  // remote and synapse: required; the model the service serves.
      // "endpoint": "https://host/v1",   // remote only: base URL; AFT appends /rerank
      // "api_key_env": "RERANK_API_KEY",  // remote only: env var holding the key (not the key)
      "top_n": 20,                // results reranked per search (default 20, at most 200; the
                                  //   backend caps it too: 64 for onnx, 20 for remote and synapse)
      "timeout_ms": 1500          // budget per search (default 1500, clamped to 50..15000)
    }
  }
}
```

What gets reranked:

- **Prose questions only.** A query that is wholly an identifier, a path, a regex, an error code or one quoted literal is not reranked; it keeps the fused order and its response carries no rerank note. Mixed queries (prose words around an identifier) are reranked.
- **Exact matches keep their order.** Results with exact evidence keep the engine's order and positions; only the first `top_n` non-exact results of the first result block are reordered, and only among the positions they already held. Results below them are untouched.
- Each candidate is scored as `path:line`, a name line (the symbol's name, or for a whole-file result the query's identifier found in the file, else the file name), and up to about 1 KB of its text.
- Scoring never blocks a search. While a backend is still being built, or when it times out, is busy or fails, the search keeps the fused order and appends `(rerank skipped: <reason>)` to the response. The first outcome for a result list (an order or a skip) is reused for every later page of that list, so pages never repeat or lose results.

**Trust boundary:** `search.rerank` is user config. A project config may only set `"backend": "off"`; any other project-tier rerank key, including `backend` set to something else, `model`, `endpoint`, `api_key_env`, `top_n` and `timeout_ms`, is dropped with a configuration warning. The same applies to a project `harnesses.<name>.search` block. A repository can turn reranking off for itself, but it cannot turn it on or point it at a server.

Backends:

- **`synapse`** runs the model in CortexKit Synapse. It needs AFT running under the CortexKit daemon with Synapse registered; outside it the backend reports itself unavailable and searches keep the fused order. `model` names the Synapse rerank model.
- **`onnx`** runs a pinned model locally through ONNX Runtime. The model is downloaded and hash-checked in the background on first use (about 0.15 to 2.3 GB depending on the model), and searches are not reranked until it is loaded. It is CPU-heavy: about 2 s and several CPU-seconds per reranked search on a laptop, more when the machine is busy, so it suits idle machines. With the default `timeout_ms` a busy CPU often skips instead of reranking.
- **`remote`** calls a Cohere-style `POST <endpoint>/rerank` (`model`, `query`, `documents`, `top_n` in; `results[{index, relevance_score}]` out, scores in [0, 1]). This works with OpenRouter, Voyage, Cohere and other compatible services, and with a Hugging Face TEI server when the endpoint carries the `tei+` prefix (`tei+http://host:8080`). At most 20 candidates are sent per search. Redirects are refused, and errors never include the key or response bodies.

See [Synapse search backends](synapse-search-backends.md) for the wire details of `remote` and `synapse`.

## Reading at a git revision

`revision_reads: true` adds an optional `ref` argument to `read`, `aft_outline` and `aft_zoom`
that reads a file as it is at a git revision (a branch, tag, SHA, or a form like `HEAD~2`)
straight from the repository's object store. It defaults to `false`. While it is off the
argument appears in no tool schema or description, and a call that passes it anyway is refused
with `revision_reads_disabled`. User and project configs both accept the key, with the usual
project-over-user precedence: the feature grants nothing beyond what the read permission rules
already allow, because the path is checked exactly like a working-tree read.

The content never comes from the working tree, and AFT runs no git command that writes the
working tree, the index or HEAD. Each call resolves the revision once and names the commit in
its reply. See [Reading at a git revision](tools.md#reading-at-a-git-revision-ref) for the
argument's behavior.

## GitHub integration

The user-only `github` block controls the complete GitHub surface. `shim` defaults to `true`; `read` and `write` default to `false`. Set all three to `false` for zero AFT-originated `gh` traffic. GitHub capabilities never unregister host tools. Project `github` blocks are ignored with a configuration warning because repositories cannot grant themselves network-backed capabilities or vary host-wide tool descriptions.

```jsonc
{
  "github": {
    "shim": true,
    "read": true,
    "write": false
  }
}
```

`github.write: true` implies read: with `github.read` absent or `false`, read is still enabled and a warning names both keys. This prevents edits from addressing a comment ordinal the agent cannot inspect. Untrusted MCP and forced-restrict binds treat every GitHub integration as disabled regardless of user configuration.

AFT maintains `<storage_root>/shims/gh` (or `gh.cmd` on Windows) and prepends that directory only to governed child processes. The shim routes eligible commands through the existing manifest, identity, classification, and refusal path before calling the first real `gh` later on `PATH`; the operator's shell startup files and terminal `PATH` are never changed. The advanced `gh_shim.binary_path` setting still selects an absolute development or deployed AFT image; a `gh_shim` block containing only `binary_path` loads normally. The retired `gh_read` block and the `gh_shim.enabled` leaf are rejected immediately with `removed_config_key:gh_read:use:github.read` and `removed_config_key:gh_shim:use:github.shim`; the whole configuration is then not used until `npx @cortexkit/aft doctor --fix` rewrites them.

When reads are enabled, `read`, `aft_outline`, and `aft_zoom` accept `issue://NUMBER` and `pr://NUMBER`, including `issue://OWNER/REPO/NUMBER` and `pr://OWNER/REPO/NUMBER`. When writes are enabled, `write` on a base resource publishes a conversation comment after the host's edit-class permission prompt displays the exact body. `edit` accepts only an `edits[]` find/replace request on `issue://.../comments/K` or `pr://.../comments/K`; it fetches the live body, applies the normal matcher, and then attempts an id-addressed edit through the governed shim. Review-thread comments are not supported. GitHub comment mutations do not create aft_safety snapshots and cannot be undone through aft_safety.

Every enabled GitHub resource read fetches live data; a prior read never satisfies a later request by itself. Successful live renders are retained only as a fallback copy, scoped to the resolved resource and authentication identity. If a live fetch fails and that copy exists, AFT returns it with this exact first-line disclosure before the rendered document:

```text
[cached copy from <ISO8601 UTC>; live fetch failed: <short reason>]
```

If no fallback copy exists, the live-fetch error is returned unchanged. Successful structured `gh` mutations invalidate matching fallback copies, and concurrent reads of the same resource share one in-flight live fetch.

## OpenCode read rules and search permissions

On OpenCode 1 and 2, `read` permission rules govern `read`, `aft_outline` and
`aft_zoom`. Outline and zoom ask for each file whose content they return, including
files read at a git revision. A single denied file is refused; batches leave out
denied entries and report their permission errors while returning allowed entries.
URL targets do not use file-read permission.

`grep` and `aft_search` are governed by their own permissions. They can return
matched lines from files denied by a `read` rule. Use path restrictions or deny
rules on the search tools to keep secrets out of results; do not rely on a `read`
deny to hide a file from search.

## OpenCode 2 permission prompts

When OpenCode 2's rules resolve a tool call to "ask", AFT calls the host's in-process `permission.assert` and waits for its answer. Configured allow/deny decisions keep their fast path; edit requests still carry `metadata.diff` for the built-in diff view. Denials and feedback are returned as permission failures; rejecting without feedback ends the current step.

**OpenCode 2.0.29 or newer is required.** A V2 context without `permission.assert` is refused at startup rather than silently losing permission-gated tools. Prompts work without HTTP discovery or a readable server password, including standalone hosts and passwordless `opencode serve` launches.

The removed `opencode.server_url` and `opencode.server_password_env` settings now fail configuration loading with `removed_config_key`. Run `npx @cortexkit/aft doctor --fix` to remove them; no replacement setting is needed.

## OpenCode 2 background wakes over subc

With `subc.connection_file` set, background shell completions and pattern matches wake their owning OpenCode 2 session through a session-scoped subscription. Notifications remain unacknowledged until the host admits the wake.

After a Location reload or host restart, AFT restores subscriptions in the background for up to 20 of that Location directory's newest sessions whose last update was within 24 hours. Recovery is sequential and does not delay Location startup or inspect another workspace's sessions. Sessions outside that window or cap recover on their next tool call or prompt.

## Git co-authorship

`git.co_author` controls commit attribution for AFT-spawned agent children. `"off"` is the default, `"auto"` derives the repository's bound agent from the gh-shim manifest and cached GitHub numeric ID, and an explicit `"Name <email>"` value is used verbatim. When enabled, AFT selects a complete dispatcher set through child-only `GIT_CONFIG_*` variables; it does not edit global or repository Git configuration. The dispatchers are identical for every project and storage root, so AFT writes them once per user to a directory named by a hash of their content, `<aft-cache>/git-hooks/<content-hash>`, where `<aft-cache>` is `AFT_CACHE_DIR` when set, otherwise `%LOCALAPPDATA%\aft` on Windows and `$XDG_CACHE_HOME/aft` or `~/.cache/aft` elsewhere (`<storage_root>/git-hooks/<content-hash>` when no home directory is known). The set is created atomically and never rewritten while intact, so each hook version is a single new executable per user. Each dispatcher preserves arguments, stdin, and exit status while chaining to the first executable repository hook from local `core.hooksPath`, the repository's Git directory, or `.githooks`. The `prepare-commit-msg` dispatcher adds attribution first so the repository hook can validate or amend it. AFT quarantines unknown or modified entries in its managed directory and regenerates the expected dispatchers before child launch. Project config may override this attribution key because attribution is not a trust boundary.

## Remote runs

Whole-line `runon` additionally requires `bash.runon_enabled: true` in the user config. This live safety switch defaults to false, ignores all project values, and can be changed without restarting AFT. It does not stop deployed worker plans' enabled `commands` prefix routing; those plans retain today's literal matcher even while whole-line runon is off.

`remote_exec.enabled: true` in your user config offers bash's `runon` argument to OpenCode and Pi sessions running in subc mode on macOS or Linux: a call with `runon: "linux"` runs its whole command line on the remote Linux build server instead of this machine (see [bash](tools.md#bash)). Standalone NDJSON sessions never register the parameter, even with this setting enabled. Remote-build guidance is shown only alongside an available parameter; default catalogs show neither. Provider availability is checked at call time, and a missing provider is refused by name without a local run. Nothing runs remotely unless a call asks for it. `remote_exec.default_demand` names the runner demand a `runon` call without specifics runs under; it never makes a call remote by itself.

Both keys are honored only in user config. A project config may set `remote_exec.enabled: false` to turn remote runs off for that project, and then a `runon` call there is refused with `remote runs are off for this project`; a project value of `true`, or any `default_demand`, is ignored with a warning. Whether `runon` is offered is decided when the session starts, so a change takes effect after a restart. Broca workers take the same two fields from their plan's `remote_exec` item instead of this config; an older plan's `remote_exec.commands` list is accepted and ignored.

## Native command sandbox

`sandbox.profile` selects a shell profile by authenticated session role. Put user choices in `~/.config/cortexkit/aft.jsonc` (or the absolute XDG config location):

```jsonc
{
  "sandbox": {
    "profile": { "worker": "relaxed", "head": "off" }
  }
}
```

These are the defaults. Stamped Alfonso workers use `worker`; heads, unstamped routes, standalone invocations and default sessions use `head`. Untrusted binds, including `mcp:*` and unverified principals, always use `strict`, even when both configured roles are `off`. A project may only harden a role (`off` < `relaxed` < `strict`), never loosen the user's choice. Project `read_deny` entries accumulate; `write_allow` is user-only and adds grants to `strict`, not exceptions to `relaxed`'s fixed denies. Live changes apply to new commands; running tasks keep their original profile.

- **`off`** installs no OS sandbox. AFT's file-tool permission asks still apply. Governed `gh`, Git co-author hooks and credential stripping are independent of the OS policy; Unix worker launches retain the short private task temporary directory even with this profile.
- **`relaxed`** is allow-by-default on macOS, with the fixed denies below. Caches, `~/Library` outside protected paths, `/tmp`, `/var/folders`, Mach services, network access, browsers, Xcode and Unix sockets remain available. Shared compile servers such as sccache and configured rustc wrappers are allowed.
- **`strict`** retains the existing native jail: writes are limited to the project, private task temp, configured write grants and build caches. Stamped workers additionally get narrowly scoped shared Git metadata and operator-configured build-target grants. Their sccache channels and rustc wrappers remain blocked because a compile server could write outside that jail. Strict's existing secret floor, socket denies and platform limitations are unchanged.

Sandboxed commands receive `/private/tmp/aft-w/<hex>` on macOS (`/tmp/aft-w/<hex>` on Linux) through `TMPDIR`, `TMP` and `TEMP`. The AFT-managed `gh` shim (the command wrapper that routes GitHub operations through AFT's permission checks) stays first on the command PATH, Git co-author hooks still run, and child environments do not inherit daemon credentials.

**Worker signals:** on macOS, worker commands in both `strict` and `relaxed` can signal only processes that the same command started. On Linux, both modes enforce this limit only when the kernel supports Landlock signal scoping (ABI 6 or newer); older kernels do not enforce it, and the command reports `signal_scope=not_enforced`. To stop a process from an earlier command, use `bash_kill`. On macOS a `kill -0` existence check of any process still works. On Linux kernels that enforce the limit, Landlock has no exception for it, so `kill -0` of a process outside the command fails with `EPERM`. That error still means the process exists; only `ESRCH` means it is gone.

**Worker `gh` fence:** strict Linux/macOS and relaxed macOS worker shells deny a canary under the account home returned by the password database, and under the daemon's `HOME` when its canonical path differs. Launch refuses if the password-database account home cannot be resolved or its canary cannot be prepared, a grant overlaps a canary, or any canary remains readable after sandbox application. Head shells retain read access to both canaries. macOS permits only marker/ancestor metadata for the probe, not data reads or writes.

**No proven fence on `off`, relaxed Linux, or Windows.** These modes report `operator gh fence: unenforced` in their unenforced list. Their ticket and worker environment markers still refuse native `gh` while present, but clearing the environment can remove that protection. Do not treat these modes as an environment-proof worker boundary. An outer sandbox alone does not identify a worker. If it also denies the same canary file, the shim may mistake that denial for AFT's worker-jail fence.

The canary governs dispatch through the shim only. Running the upstream `gh` binary directly bypasses the shim. Credential files, environment tokens, keychain services, and requests to launch commands through host services (for example, launchd or systemd) require independent sandbox policies. Denying keychain files alone does not prove that login-keychain service queries are blocked.

Strict native profiles and relaxed macOS profiles enforce credential-file denies for both the daemon's `HOME` and the password-database account home, subject to the Linux limitations below. Relaxed Linux includes these paths in its unenforced report instead. GitHub config denies cover each home's `.config/gh` and configured `GH_CONFIG_DIR` and `XDG_CONFIG_HOME/gh` paths; defaults remain protected when overrides are set. SSH, cloud CLI, GnuPG, netrc, Git credential-store, Docker, npm, PyPI, Kubernetes and Cargo credential paths follow the same home handling. Relative overrides resolve from the checkout, and both symlink aliases and targets are protected. Unresolvable credential paths or failures to inspect an existing Cargo credential directory refuse the launch rather than silently dropping part of the floor.

**macOS setuid limitation:** Seatbelt refuses to execute setuid tools, including `/bin/ps` and `top`, even with an allow-default profile. `pgrep` and `pkill` work in sandboxed shells. Select `off` when a command genuinely needs `ps`; AFT does not add an unsandboxed-exec exception or copy platform binaries to evade this restriction.

### Relaxed deny list (macOS)

Read and write protection covers:

- `~/.ssh`, `~/.gnupg`, `~/Library/Keychains`, `~/.aws`, `~/.config/gcloud`, `~/.azure`, `~/.config/gh`, `~/.netrc`, `~/.git-credentials`, `~/.config/git/credentials`, `~/.docker/config.json`, `~/.npmrc`, `~/.pypirc`, `~/.kube/config` and `~/.cargo/credentials*` (including future filename suffixes).
- CortexKit config, data and state: `~/.config/cortexkit`, `~/.local/share/cortexkit`, `~/.local/state/cortexkit`, their absolute XDG equivalents, AFT's resolved storage and the session store. This includes other agents' private stores. Daemon connection files selected through trusted configuration, `SUBC_CONNECTION_FILE`, `XDG_RUNTIME_DIR/subc-connection.json` and the production runtime path are protected even outside those trees. User `sandbox.read_deny` paths are protected as well.

Private-store read exceptions are the checkout pool (so sibling checkout reads work), the current checkout and task I/O/temp, placed CortexKit binaries (`~/.local/share/cortexkit/bin` and the XDG equivalent), and the exact shim executable, `<storage>/shims`, content-keyed Git hook directory, and Git-resolved metadata/hook paths. AFT storage defaults to `<XDG_DATA_HOME>/cortexkit/aft` or `~/.local/share/cortexkit/aft`; [storage overrides](#storage-root-environment-override) can relocate it. The session store is the task directory under `<storage>/bash-tasks/<session-hash>`. These exceptions do not expose other agents' stores. Sandboxed shells can also read the file-tool-readable saved command files in their own harness/session (including local captures of remote commands), never saved launch environments or other private storage; macOS matches only those filenames, while Linux strict snapshots existing files from the 500 most recently modified task entries at launch and leaves older or later-created files unreadable until a subsequent eligible snapshot, without adding write access beyond the current task's existing I/O grant.

Additional write denies cover:

- `~/Work` and the Alfonso worktree pool, except the current checkout and the narrow shared Git object/ref/log/packed-ref grants needed for commits and branch deletion. Registered main and sibling worktrees outside those trees are also protected. This is a known-checkout boundary: a previously unknown, unrelated checkout outside these locations is not discovered by a filesystem-wide scan.
- CortexKit config/data/state and AFT storage, except the current checkout and the current task's AFT-owned I/O/temp/scratch. Managed shim and hook executables remain immutable.
- `~/.zshrc`, `~/.zprofile`, `~/.bashrc`, `~/.profile`, `~/.gitconfig`, `~/.config/git` and `~/Library/LaunchAgents`.
- The repository's Git `config`, linked `config.worktree`, default and effective hooks directories, core's hook stubs, and the resolved `mason.originalHooksPath` target. Git binding files and executable project policy (`.cortexkit/aft.jsonc`, `.cortexkit/aft/filters`) remain protected too.

Deny targets are re-canonicalized at launch, including missing/dangling targets; relaxed also retains alias paths and matches ASCII case aliases for protected names on case-insensitive volumes. Write protection includes replacing enclosing governance directories, not just overwriting their files.

### Linux and Windows guarantee boundary

**Linux `relaxed` does not enforce these denies.** Landlock grants are additive: root-wide read/write grants cannot subtract a credential, sibling checkout, config file or hooks subtree. To preserve default-allow semantics, relaxed grants broad access and prints every unenforced read and write deny by path, including the `~/.cargo/credentials*` filename prefix, in `sandbox-launch: profile=relaxed unenforced=[...]`. It must not be treated as a secret or write boundary on Linux. No extra sandbox dependency is installed.

Linux `strict` retains the existing Landlock allowlist and requires ABI V3 or newer. Invalid profiles, unavailable kernels and writable grants that overlap mandatory credential/private-store denies fail closed with `sandbox_unavailable`: Landlock cannot subtract those secrets from a writable parent. The strict guarantee applies to canonical paths without pre-existing aliases into a granted tree:

- Nested write denies inside writable roots cannot protect `.git/hooks`, `config.worktree` or `.cortexkit`; each limitation is reported. `REFER` is granted only with writable roots to prevent newly created hard links from widening access. Pre-existing hard links, bind mounts, case-insensitive/overlay aliases and pre-opened descriptors remain outside this guarantee.
- Unix-domain socket connects are not mediated. TCP/DNS remain open; strict's default/operator-selected sccache loopback ports are reported as unenforced because blocking only those destinations would require restricting all other ports too.
- Project reads may be split around nested denies using children present at launch. Newly created root-level files may remain unreadable until the next command recomputes grants. Shared `/tmp` and sensitive runtime trees are not granted by strict.
- `/proc` is readable for tooling. With Yama `ptrace_scope=0`, another same-UID process can expose environment or memory; missing/unreadable Yama configuration produces a conservative exposure warning.
- In Linux strict, Cargo credential files found by the launch-time scan and their resolved symlink targets receive denies. Landlock cannot express credential filename-prefix denies, so future matching filenames are reported as `credential_filename_prefix_future_entries_unenforced_on_landlock` rather than claimed protected by a prefix rule. Relaxed Linux reports the entire credential floor as unenforced.

Windows has no OS sandbox backend. `relaxed` runs with full user rights and reports `profile=relaxed unenforced=[all filesystem read/write denies,operator gh fence: unenforced]` in captured output, including PTY output. Like Linux relaxed, it is **not** a secret or write boundary. `strict` refuses execution with `sandbox_unavailable`: `sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux`. Untrusted and MCP routes are forced strict and therefore refuse. `off` runs without an OS sandbox; worker commands report the unenforced `gh` fence, while head commands remain quiet.

### Retired `sandbox.enabled`

Loading translates this retired key in memory, and `doctor --fix` uses the same translation while preserving JSONC comments and unrelated keys. User config can be automatically migrated with a backup; shared project config is not rewritten automatically.

- `true` becomes `{ "worker": "strict", "head": "strict" }`.
- `false` (or an absent key) uses the new defaults, `{ "worker": "relaxed", "head": "off" }`.
- Explicit `sandbox.profile` role values take precedence over the legacy boolean.

Heads previously ran without an OS sandbox unless `sandbox.enabled: true` was set. Thus default head `off` preserves default head behavior, while the translation preserves confinement for explicit `true`. The old boolean never controlled the forced stamped-worker jail; explicit `false` does **not** pin workers to strict after this cutover.

## Config schema migration

### Feature-based configuration (v0.58)

v0.58 replaces surface levels with one rule — a tool is registered unless it is
in `disabled_tools` — and makes the background indexes first-class
(`indexes.trigram`, `indexes.semantic`, `indexes.callgraph`, all default on).
Retired keys are never refused. On every load AFT translates them into their
current equivalents, and those then follow the usual user/project rules:

- **User file** (`~/.config/cortexkit/aft.jsonc`): AFT rewrites the file to the
  current keys the first time it reads it, with the same comment-preserving
  migration `npx @cortexkit/aft doctor --fix` performs. The previous text is
  kept beside it as `aft.jsonc.bak-<unix seconds>`, and one notice says the
  file was updated. A read-only file, or one AFT cannot write, is left alone:
  the keys are translated in memory and the notice says why the file was not
  updated.
- **Project file** (`<project>/.cortexkit/aft.jsonc`, including its
  `harnesses` blocks): it is shared through the repository, so AFT never
  writes it. The keys are translated in memory with the same limits a project
  config has for the current key (for example, a project's `gh_read` becomes
  `github.read`, which a project cannot set, so it is ignored), and one notice
  names the file and its retired keys. Run `npx @cortexkit/aft doctor --fix`
  in the project to update the file.

| Retired input | Replacement |
| --- | --- |
| `tool_surface`, `hoist_builtin_tools`, `enabled` | `disabled_tools` |
| `search_index`, `experimental_search_index` | `indexes.trigram` |
| `semantic_search`, `experimental_semantic_search` | `indexes.semantic` |
| `callgraph_store` | `indexes.callgraph` |
| `github.enabled` | `github.read`, `github.write`, `github.shim` |
| `gh_read.enabled`, `gh_shim.enabled` | `github.read`, `github.shim` (`gh_shim.binary_path` stays) |
| `idle.lsp_ttl_minutes` | `lsp.idle_minutes` |
| `inspect.tier2_soft_deadline_ms`, `inspect.max_drill_down_items` | removed (they had no effect) |
| `bridge.hang_threshold` | removed (no effect: see [Bridge request timeouts](#bridge-request-timeouts)) |
| `aft_read`, `aft_write`, `aft_edit`, `aft_apply_patch`, `aft_grep`, `aft_glob`, `aft_bash` in `disabled_tools` | `read`, `write`, `edit`, `apply_patch`, `grep`, `glob`, `bash` |

Translation rules: an explicit `tool_surface` in the user base is a complete
registration choice (`"all"` → `[]`, `"recommended"` → `["aft_callgraph",
"aft_delete", "aft_move"]`, `"minimal"` → everything except `aft_outline`,
`aft_zoom` and `aft_safety`); without one, the default `["aft_move",
"aft_delete"]` is united with names generated by other legacy gates.
`hoist_builtin_tools: false` generates the seven host names; top-level
`enabled: false` generates every tool. It no longer turns AFT off: the
trigram, semantic and callgraph indexes still build, and the migration notice
says so. To keep AFT from indexing a repository, also set `indexes.trigram`,
`indexes.semantic` and `indexes.callgraph` to `false`. False `backup.enabled`,
`inspect.enabled` and `bash`/`bash.enabled` only switch their behaviour off;
they no longer remove tool registrations (a load with one and no
`disabled_tools` warns `legacy_runtime_gate_runtime_only`), so list the tools
in `disabled_tools` to unregister them. An explicit `disabled_tools` in the same
block, including `[]`, wins over every generated name. Project configs cannot
disable `aft_safety` or host tool slots, whether directly or through a legacy
key.

A configuration AFT cannot use — a file that does not parse or a missing
`subc.connection_file` — no longer falls back to defaults. The plugin still loads, every AFT tool call
returns the error and how to fix it, the sidebar and status show it, and no
indexing starts until the file is fixed and the host restarted.

When `FASTEMBED_CACHE_DIR` is not set, the local embedding model is cached in
`$XDG_CACHE_HOME/fastembed` if `XDG_CACHE_HOME` is an absolute path, otherwise
`~/.cache/fastembed`.

### Earlier migrations

v0.18 reorganized experimental flags. Old config files using the flat shape:

```jsonc
{
  "experimental_search_index": true,
  "experimental_semantic_search": true,
  "experimental_lsp_ty": true,
  "experimental_bash_rewrite": true,
  "experimental_bash_compress": true,
  "experimental_bash_background": true
}
```

had their `experimental_lsp_ty` and `experimental_bash_*` keys migrated
automatically on first load to the v0.18 shape (the two index keys are now
translated as described above instead of being rewritten):

```jsonc
{
  "experimental": {
    "lsp_ty": true,
    "bash": { "rewrite": true, "compress": true, "background": true }
  }
}
```

The original file is rewritten in place (both `.jsonc` and `.json` candidates are migrated).
JSONC comments are preserved. Both user-level and project-level configs are migrated
independently. The migration is idempotent — running again is a no-op.

**v0.27.2** further graduated the bash flags out of `experimental`. A config still using
`experimental.bash.{rewrite,compress,background}` is read transparently as a fallback, but the
canonical shape is the top-level `bash` block shown above. `experimental` now holds only
`lsp_ty`.

## Language servers (LSP)

`lsp.idle_minutes` is **minutes since the last AFT tool call on that repository**,
not time since the language server last emitted a diagnostic. It defaults to
`60`; integer values are clamped to `5..=1440`. Set it to `"never"` to disable
idle reaping. This does not prevent shutdown on unbind, eviction or daemon
shutdown, and is independent of `idle.root_ttl_minutes`. Reaped servers respawn
when next needed. The same activity definition applies to standalone and subc.

This is a **tighten-only resource setting**: project config may lower the user's
number, or choose a number when the user chose `"never"`. A larger project value
or `"never"` over a user number is ignored and reported as a dropped key by the
Rust resolver. Both plugins apply the same floor. The default `60` is the floor
when the user did not set it. Active harness overrides obey their tier's trust
rules. Changes apply live, at the next request/maintenance boundary, without
restarting AFT. A project edit cannot remove an already published tightening
until reconnect; trusted user edits may change the user budget in either direction.

`inspect.categories` has exactly seven boolean keys: `diagnostics`, `todos`,
`dead_code`, `unused_exports`, `duplicates`, `cycles`, `complexity`. All default
to `true`. A false category is not scanned, built or refreshed, for scoped or
unscoped inspections; it emits only e.g. `dead code: off (inspect.categories.dead_code)`.
It cannot make the header PARTIAL and does not show a cached findings count.
Status bars use **`○` for off** (e.g. `D○`), distinct from `?` for unknown and
`0` for verified clean. Metrics remains internal and always computed for scoped
file counts; it is not a configurable or rendered category. Project config can
turn categories off, but cannot turn on a category the user turned off. Category
changes apply live; work already admitted retains its pinned configuration.
`inspect.enabled: false` remains the whole-tool runtime switch.

`idle.lsp_ttl_minutes` is replaced by `lsp.idle_minutes`.
`inspect.tier2_soft_deadline_ms` and `inspect.max_drill_down_items` were inert and
are removed (`inspect.tier2_pass_timeout_ms` and `aft_inspect.topK` are what they
used to approximate). These old keys are retired like the ones in
[Feature-based configuration](#feature-based-configuration-v058), including inside
harness blocks: loading moves the old idle value to `lsp.idle_minutes`, clamped
to the new range (an existing canonical value wins, and a project file may still
only shorten the window), and drops the two inert inspect keys. The user file is
rewritten that way automatically; `npx @cortexkit/aft doctor --fix` rewrites a
project file.

AFT runs language servers in-process for post-edit diagnostics and on-demand `lsp_diagnostics`
calls. Servers are spawned lazily — only when a file matching their extensions is touched, and
only if their binary can be resolved from project `node_modules/.bin`, AFT's managed cache, or
`PATH`. Python-family servers additionally check the selected nested workspace's `.venv` or
`venv` first.

AFT sets `GIT_OPTIONAL_LOCKS=0` for every language server so Git status calls by server descendants skip optional index locks; mandatory Git write locks are unaffected.

**Built-in servers** (auto-registered, no config needed):

| Server | Languages | Binary |
|---|---|---|
| TypeScript Language Server | `.ts .tsx .js .jsx .mjs .cjs` | `typescript-language-server` |
| Pyright | `.py .pyi` | `pyright-langserver` |
| rust-analyzer | `.rs` | `rust-analyzer` |
| gopls | `.go` | `gopls` |
| bash-language-server | `.sh .bash .zsh` | `bash-language-server` |
| yaml-language-server | `.yaml .yml` | `yaml-language-server` |
| Dockerfile Language Server | `.dockerfile` | `docker-language-server start --stdio` (preferred); `docker-langserver --stdio` fallback |

Docker's `docker-language-server` is preferred when available and runs with `start --stdio`.
The npm `docker-langserver` server remains a fallback; AFT's plugins continue to auto-install
`dockerfile-language-server-nodejs` when `lsp.auto_install` is enabled.

**TypeScript 7 and later** ship no `tsserver.js`, so `typescript-language-server` cannot
serve them. When the nearest installed `node_modules/typescript/package.json` reports version
7 or later, AFT starts the compiler's own language server instead: the platform package's
`@typescript/typescript-<os>-<cpu>/lib/tsc --lsp --stdio`, run directly rather than through the
Node wrapper in `node_modules/.bin`. Its id is `typescript-native`. Each TypeScript 7
installation gets its own server, separate from `typescript-language-server`, so a monorepo that
mixes TypeScript 5 and 7 packages runs
both. The choice follows what is installed, never the lockfile. `lsp.disabled: ["typescript"]`
turns both off; `typescript-native` turns off only the native server. If the platform package is
missing, or the installed version is unreadable, AFT reports a named gap and starts nothing.
The built-in `typescript-language-server` is only swapped when you have not pointed the
`typescript` server at another binary.

**Experimental:** `ty` (Astral's Python type checker) — gated behind
`experimental.lsp_ty: true` or `lsp.python: "ty"`. When enabled, ty runs alongside Pyright
unless you also disable Pyright via `lsp.disabled: ["python"]` (or use `lsp.python: "ty"`
which does both automatically). Python-family servers first look for their binary in the selected
nested workspace's `.venv` or `venv`, then its `node_modules/.bin`, the configured project root's
`node_modules/.bin`, AFT's managed cache, and `PATH`. While ty remains alpha,
`lsp.python: "auto"` stays on Pyright rather than silently changing diagnostic semantics based on
which binaries happen to be installed.
For Pyright, AFT also returns the selected virtualenv interpreter through Pyright's
`workspace/configuration` request so imports resolve against that environment.
Project-local language-server binaries and the interpreter selected for Pyright can execute with
the user's privileges; enable LSPs only for projects and virtual environments you trust.

**Registering a custom server:** add it under `lsp.servers` in your config. The example
configuration above shows registering `tinymist` for Typst files. Required fields per server:
`extensions` (array, leading `.` is stripped), `binary` (PATH lookup name). Optional:
`args`, `root_markers` (defaults to `[".git"]`), `disabled`.

**Disabling a built-in:** add the server's id to `lsp.disabled`. Built-in ids are
`typescript`, `typescript-native` (TypeScript 7+), `python` (Pyright), `rust` (rust-analyzer), `go` (gopls), `bash`,
`yaml`, and `ty`. Custom servers use the key you registered them under in
`lsp.servers`. IDs are case-insensitive.

**Custom server fields:**

| Field | Required | Description |
|---|---|---|
| `extensions` | yes | Array of file extensions (leading `.` is stripped) |
| `binary` | yes | Binary name resolved against `PATH` |
| `args` | no | Args passed to the server (default: `[]`) |
| `root_markers` | no | Filenames whose presence anchors the workspace root (default: `[".git"]`) |
| `env` | no | Extra environment variables for the spawned process |
| `initialization_options` | no | Passed to the server's LSP `initialize` request; recursively merged over built-in options. Objects merge, while explicit arrays and scalars replace built-in values (including an empty array). |
| `disabled` | no | Skip this server even though it's registered |

Rust-analyzer's automatic diagnostic checks (flycheck) no longer run `cargo check`
on save or workspace reload by default.
Its in-memory analysis remains enabled; `aft_inspect` runs an explicit compiler
check when needed and reuses a saved result while the checkout inputs are unchanged.
To restore automatic checks, set the following in your **user** config:

```jsonc
{ "lsp": { "servers": { "rust": {
  "initialization_options": { "checkOnSave": true }
} } } }
```

This keeps the built-in Cargo `--locked` arguments. To override those arguments,
set `cargo.extraArgs` or `cargo.metadataExtraArgs` explicitly, for example to `[]`.

**Missing-tool warnings:** on startup, AFT detects configured-but-missing formatters, type
checkers, and LSP binaries (for languages your project actually uses) and surfaces a one-time
notification per warning through whatever notification channel the harness exposes (OpenCode's
ignored-message channel, Pi's status messages). Dismissed warnings do not re-fire on plugin
updates — dedupe is per-warning-content, persisted in `<storage_dir>/warned_tools.json`.

## LSP auto-install

AFT auto-installs language servers your project actually needs. npm-distributed servers are
installed with `npm install --no-save --ignore-scripts` into AFT's cache (works under Node-only
hosts, no Bun required); standalone binaries (clangd, lua-ls, zls, tinymist, texlab) download from
GitHub releases. The cache lives at `~/.cache/aft/lsp-packages/` and `~/.cache/aft/lsp-binaries/`
(Windows: `%LOCALAPPDATA%/aft/...`).

Configure via `lsp.*`:

```jsonc
"lsp": {
  // Auto-install relevant language servers on plugin startup. Default: true.
  // Set false to require manual install (servers still work if on PATH).
  "auto_install": true,

  // Supply-chain grace window in days. AFT only installs versions that have
  // been on the registry / GitHub releases for at least this many days,
  // defending against newly-published malicious versions that get yanked
  // within hours of detection. Default: 7. User pins via `lsp.versions`
  // bypass this.
  "grace_days": 7,

  // Per-package version pin map. Pins bypass the grace filter.
  // Keys: npm package name OR `owner/repo` for GitHub-hosted servers.
  "versions": {
    "typescript-language-server": "5.0.0",
    "clangd/clangd": "21.1.0"
  }
}
```

**Trust boundary:** `lsp.auto_install`, `lsp.grace_days`, `lsp.versions`, `lsp.servers`, and
`lsp.disabled` are **user-only** — values from project config (`<project>/.cortexkit/aft.jsonc`)
are stripped on load. A hostile repository cannot weaken your supply-chain
defenses, redirect AFT to download a different binary, or silently disable LSPs you rely on.
The plugin logs a warning when it strips a project-level setting.

**Trust-On-First-Use (TOFU) verification:** AFT records the SHA-256 of every downloaded
GitHub release archive in `.aft-installed`. If the same tag is ever re-installed with a
different hash, AFT refuses the install and points to `aft doctor --clear` for manual
recovery. The hash is also logged to the plugin log on every install for forensic comparison
against published checksums.

**What we do not do (yet):** AFT does **not** ship a vetted checksum allowlist. The TOFU
defense above only protects against post-cache-warmup tampering; the very first install of
any tag is accepted as-is once it passes the grace window and TLS verification. Supply-chain
attacks faster than the grace window are a residual risk. A fully-vetted allowlist is on the
roadmap.

## Bridge request timeouts

`bridge.request_timeout_ms` (user config only, default 30000) is the transport
budget for one request to the shared `aft` process. A request that runs out of
it fails on its own, with a note that a timed-out edit may still have been
applied; other requests and the shared process are not affected. Timeouts never
restart the shared process. Its hang watchdog restarts it when a request sent
since the process last wrote a message has gone unanswered, with no message of
any kind from the process, for twice the longest budget among those requests and the calls still
waiting (at least 60 seconds). This happens even if every caller has already
timed out, so the next call gets a fresh process. A call to a read-only
tool (`read`, `outline`, `zoom`, `grep`, `glob`, `search`, `callgraph`,
`inspect`) cut off by that restart is retried once, unless it was cancelled.

`bridge.hang_threshold` used to count timeouts toward that restart and no longer
does anything. It is retired like the keys in
[Feature-based configuration](#feature-based-configuration-v058): loading drops
it with a one-line notice, the user file is rewritten automatically, and
`npx @cortexkit/aft doctor --fix` removes it from a project file.

## Durable logs and performance ticks

AFT keeps its own logs under `<storage_root>/logs/`. The storage root follows the
same resolution as indexes and other persistent data: `AFT_STORAGE_DIR`, then
`XDG_DATA_HOME/cortexkit/aft` when set, then the platform data directory (normally
`~/.local/share/cortexkit/aft` on Linux and macOS, or `%LOCALAPPDATA%/cortexkit/aft`
on Windows). See [Uninstall paths](#uninstall-paths) for the complete fallback order.

- Rust module processes write `aft-<pid>.log`. Each process file rolls at 32 MiB
  into one `.1` backup; files from dead PIDs are removed after 24 hours, and the
  log folder is kept under 200 MiB.
- OpenCode and Pi plugin messages share `aft-plugin.log`, which also rolls at
  32 MiB into one `.1` backup. The `[aft-plugin]` and `[aft-pi]` tags identify
  the source.
- Module lines continue to go to stderr as well, so daemon capture remains
  available while the durable files provide a module-owned history.

When AFT is active, the module emits a `perf tick:` line at most once per minute.
It summarizes watcher and drain activity, Tier-2 and semantic work, callgraph
invalidations, executor completions, and oldest queued-job ages since the prior
tick. Idle intervals stay silent. `RUST_LOG` keeps its existing env_logger
semantics and defaults to `info`.

## Working with large repositories

If you point AFT at a very large directory (monorepo root, `~/Work`, `/home`, etc.), certain
features guard against unbounded work to keep the bridge responsive:

- **Call-graph ops** (`callers`, `trace_to`, `trace_data`, `impact`) use the persisted store and
  are not capped by the removed legacy in-memory reverse-index limit.
- **Semantic indexing** is capped at `semantic.max_files` source files (default 20,000). Raise it
  when using a remote backend that embeds server-side, or lower it on memory-constrained machines.
- **`grep`, `glob`, `read`, `edit`, and other tools** work at any size.

Commands with heavier workloads get longer per-call timeouts: 60s for `callers`, `trace_to`,
`trace_data`, `impact`, `grep`, `glob`; 45s for `semantic_search`; 30s for everything else.
For best results in very large trees, point AFT at a specific project subdirectory.


## Ignoring files (`.gitignore` / `.aftignore`)

Every AFT walk — trigram index, semantic index, call graph, and `aft_inspect` —
honors `.gitignore` (including `.git/info/exclude` and nested `.gitignore`
files) and skips common build directories (`node_modules`, `target`, `dist`,
`build`, `.venv`, and similar). In a folder that is not a git repository, AFT
still applies `.gitignore` files and your global git excludes file
(`core.excludesFile`); `.git/info/exclude` only exists inside a repository.

AFT also honors an optional **`.aftignore`** file: the same syntax as
`.gitignore`, hierarchical, and working in non-git projects, layered on top of
`.gitignore`. Use it to exclude paths AFT shouldn't index that you can't put in
`.gitignore` — most commonly git submodules. Edits under an `.aftignore`d path
also stop triggering reindexing.

Naming a file explicitly in `grep` (e.g. `path: "captures/log.txt"`) searches it
even when it is gitignored or `.aftignore`d, matching ripgrep — an explicitly
named file is always searched.

## Oxlint language server

Install Oxlint in the project with `npm install --save-dev oxlint` and add an
`.oxlintrc.json` or `.oxlintrc` configuration file to enable its JS/TS diagnostics.
AFT starts `node_modules/.bin/oxlint --lsp` (supported since Oxlint 1.29.0).
If the project's `node_modules/.bin/oxc_language_server` is present, AFT prefers
that standalone server with no arguments, including when both binaries exist.
This supports older releases that lack `--lsp` without probing the version on
every start. AFT does not auto-install Oxlint; the project's dependency controls
its version. Current releases such as 1.86.0 use `oxlint --lsp` only.
