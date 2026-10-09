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

`bash.worker_wait_max_ms` bounds how long a delegated (subagent) session waits on one command before control returns to it: a `bash_watch` without a timeout, and a `bash` call that blocks until the command finishes (`wait: true`, or every foreground call when `bash.subagent_background` is false). At the limit the command is not killed: it keeps running in the background, and the worker is told it is still running, how long it has run, and its latest output, so it can wait again or `bash_kill` it. While the worker keeps waiting, each wait pushes the command's default 30-minute hard kill to at least this long after the wait, so a long build it keeps watching runs to completion, while one it stops watching is still killed. An explicit `timeout` is never extended. The default is `1800000` (30 minutes); a value below `60000` is a config error that drops the `bash` block, not a silent clamp. User and project config may both set it, and harness blocks override it like the other `bash` keys.

On Linux, user config may set `bash.linux_scope: true` to launch non-PTY tool shells through `systemd-run --user --scope --collect --quiet`. The default is `false`. AFT uses the scope only when `systemd-run` exists and the user manager is reachable; otherwise it falls back to the normal process-group-isolated spawn and writes one informational log line. Native-sandbox launches also use the normal spawn because their launcher cannot contact the user manager. Project config cannot enable or disable this host-level containment option.

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

## Uninstall paths

Delete the user and project config files listed above, then delete the data roots below. A non-empty environment override takes precedence over the corresponding default.

**Shared storage root** (`AFT_STORAGE_DIR`): if unset, AFT uses `XDG_DATA_HOME/cortexkit/aft/` when `XDG_DATA_HOME` is set. Otherwise, POSIX uses `~/.local/share/cortexkit/aft/`; Windows uses `%LOCALAPPDATA%/cortexkit/aft/`, then `%APPDATA%/cortexkit/aft/`, then `%USERPROFILE%/AppData/Local/cortexkit/aft/`. This root contains indexes, databases, background-task records, logs, and backup history.

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

  // Auto-format files after edits. Default: false. When enabled, formatting is
  // queued and runs after ~90s without further edits to the file.
  "format_on_edit": false,

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

  // Native sandbox for first-party bash and PTY commands. Default: false.
  "sandbox": {
    "enabled": false,
    // Additional writable roots. User config only.
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

  // User-only. OpenCode 2 server AFT raises permission prompts on. Absent by
  // default: AFT then finds the server it runs inside. See "OpenCode 2
  // permission prompts" below.
  "opencode": {
    "server_url": "http://127.0.0.1:4096",
    "server_password_env": "OPENCODE_SERVER_PASSWORD"  // a variable NAME, never the password
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

## OpenCode 2 permission prompts

When OpenCode 2's permission rules resolve a tool call to "ask" (a `bash` command your rules ask about, an edit outside the project, and so on), AFT raises the prompt through the OpenCode server's HTTP API. It has to find that server first, and it must be the server AFT is running inside: OpenCode 2 loads the AFT plugin into its server process, so the server showing your session is the very process AFT runs in, and that process's ID identifies it.

Without any setting, AFT finds it on its own:

- a service registration (`$XDG_STATE_HOME/opencode/service*.json`) is used only if the process ID recorded in it is the ID of the server process AFT runs in. A TUI's managed background service registers itself too; a plain `opencode serve` running beside it never sends its prompts there;
- otherwise AFT asks the operating system which TCP ports its own process listens on (`lsof` on macOS and Linux, `/proc` on Linux without `lsof`, `netstat` on Windows) and uses the password OpenCode itself adopted from `OPENCODE_PASSWORD` or `OPENCODE_SERVER_PASSWORD`. The username is always `opencode`. AFT never guesses ports: the password is only ever sent to ports this process listens on, never to other servers on `4096` and the ports after it. An explicit `--port` must be one of those ports; if the lookup itself fails, AFT uses a `--port` on the command line and otherwise refuses.

Every candidate is checked once with an authenticated call to `/api/info`, which reports the ID of the process serving it; that must be the server process AFT runs in before any prompt is sent.

Some servers cannot be found this way: `opencode --standalone` runs a private server on a random port and hides its password from plugins, and `opencode serve` started without `OPENCODE_SERVER_PASSWORD` generates a password nobody else can read. Prompts then fail closed (the call is refused, never allowed), and the refusal and the plugin log say why and how to fix it. The fix is either to start OpenCode with `OPENCODE_SERVER_PASSWORD` set, or to name the server explicitly:

```jsonc
{
  "opencode": {
    "server_url": "http://127.0.0.1:4096",
    "server_password_env": "OPENCODE_SERVER_PASSWORD"
  }
}
```

`server_url` replaces discovery. `server_password_env` names the environment variable that holds the server's password, never the password itself; when it is absent AFT uses `OPENCODE_PASSWORD`, then `OPENCODE_SERVER_PASSWORD`. Both are read when OpenCode starts.

**OpenChamber and other UIs** that start `opencode serve` for you: set `server_url` to the same host and port you gave the UI (`OPENCODE_HOST` / `OPENCODE_PORT` for OpenChamber), and make sure the server is started with a password AFT can read (for example `OPENCODE_SERVER_PASSWORD` in the environment the UI starts it in).

The `opencode` block is honored only in your user config. A project config that sets it is ignored with a warning, because a repository could otherwise send your permission prompts to a server it controls.

## Git co-authorship

`git.co_author` controls commit attribution for AFT-spawned agent children. `"off"` is the default, `"auto"` derives the repository's bound agent from the gh-shim manifest and cached GitHub numeric ID, and an explicit `"Name <email>"` value is used verbatim. When enabled, AFT selects a complete dispatcher set through child-only `GIT_CONFIG_*` variables; it does not edit global or repository Git configuration. The dispatchers are identical for every project and storage root, so AFT writes them once per user to a directory named by a hash of their content, `<aft-cache>/git-hooks/<content-hash>`, where `<aft-cache>` is `AFT_CACHE_DIR` when set, otherwise `%LOCALAPPDATA%\aft` on Windows and `$XDG_CACHE_HOME/aft` or `~/.cache/aft` elsewhere (`<storage_root>/git-hooks/<content-hash>` when no home directory is known). The set is created atomically and never rewritten while intact, so each hook version is a single new executable per user. Each dispatcher preserves arguments, stdin, and exit status while chaining to the first executable repository hook from local `core.hooksPath`, the repository's Git directory, or `.githooks`. The `prepare-commit-msg` dispatcher adds attribution first so the repository hook can validate or amend it. AFT quarantines unknown or modified entries in its managed directory and regenerates the expected dispatchers before child launch. Project config may override this attribution key because attribution is not a trust boundary.

## Remote runs

Whole-line `runon` additionally requires `bash.runon_enabled: true` in the user config. This live safety switch defaults to false, ignores all project values, and can be changed without restarting AFT. It does not stop deployed worker plans' enabled `commands` prefix routing; those plans retain today's literal matcher even while whole-line runon is off.

`remote_exec.enabled: true` in your user config offers bash's `runon` argument to OpenCode and Pi sessions running in subc mode on macOS or Linux: a call with `runon: "linux"` runs its whole command line on the remote Linux build server instead of this machine (see [bash](tools.md#bash)). Standalone NDJSON sessions never register the parameter, even with this setting enabled. Remote-build guidance is shown only alongside an available parameter; default catalogs show neither. Provider availability is checked at call time, and a missing provider is refused by name without a local run. Nothing runs remotely unless a call asks for it. `remote_exec.default_demand` names the runner demand a `runon` call without specifics runs under; it never makes a call remote by itself.

Both keys are honored only in user config. A project config may set `remote_exec.enabled: false` to turn remote runs off for that project, and then a `runon` call there is refused with `remote runs are off for this project`; a project value of `true`, or any `default_demand`, is ignored with a warning. Whether `runon` is offered is decided when the session starts, so a change takes effect after a restart. Broca workers take the same two fields from their plan's `remote_exec` item instead of this config; an older plan's `remote_exec.commands` list is accepted and ignored.

## Native command sandbox

Set `sandbox.enabled` to route first-party bash and PTY commands through Seatbelt on macOS or Landlock on Linux. Unsupported platforms, unavailable kernels, Landlock ABIs below V3, invalid profiles, and policies that cannot preserve the credential floor fail closed with a structured `sandbox_unavailable` response. Sandboxed commands receive a private task temporary directory through `TMPDIR`, `TMP`, and `TEMP`; Linux does not grant the shared `/tmp` tree.

The mandatory credential floor is `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.azure`, `~/.config/gcloud`, and `~/.config/cortexkit`. Linux canonicalizes these paths and constructs a read allowlist that omits them. A writable project, cache, temporary directory, or `write_allow` path that overlaps this floor is refused because Landlock cannot subtract write rights. Ordinary `read_deny` paths inside writable roots are supported: writes remain allowed while read grants are split around the denied path.

The floor also denies CortexKit's data and state trees (`~/.local/share/cortexkit/` and `~/.local/state/cortexkit/`, plus absolute XDG data/state locations and AFT's resolved storage). The daemon connection file is separately denied wherever the trusted `subc.connection_file` resolves, including outside those trees; `~` and relative settings resolve against HOME. Environment-selected connection files and the default runtime/production connection paths are denied too. These paths hold daemon authentication and other agents' snapshots, output, and undo history, not just the current project's data.

Only the session's project roots and the current task's private temporary directory get read/write exceptions within private trees. Other worktrees, tasks, modules, and state remain hidden. The active `gh` shim directory and its resolved executable, and the active content-keyed managed Git hooks directory, get read/execute access but never write access. Seatbelt excludes these narrow exceptions from the tree denies and allows only ancestor metadata needed for path traversal. Landlock omits the private trees from ordinary read grants and reintroduces the exact exceptions; it does not grant their parents. If a Linux writable grant encompasses a private tree, crosses the connection-file deny, or would make managed executables writable, setup fails closed with `sandbox_unavailable` explaining the overlap. This restructures the allow set rather than attempting an unsupported nested Landlock deny.

Background stdout/stderr and exit markers use descriptors opened before confinement. Native launches already disable path-based pipeline-status capture, so the shell needs no path grant to the capture files or the whole `io/` directory. Only the private temporary directory beneath `io/` is readable/writable. Linux also retains the existing exact read grants for the daemon-verified command, wrapper, and environment payload files, never their control-directory parent. `gh --status` still executes through the shim, but reports unavailable private state rather than receiving a state-directory exception.

| Protection | macOS Seatbelt | Linux Landlock |
| --- | --- | --- |
| Credential floor reads and writes | Denied | Denied by omission; overlapping writable roots are refused |
| CortexKit data/state and daemon connection file | Denied except exact project/task temp and read-only managed executables | Denied by omission with the same exact exceptions; unrepresentable writable overlaps are refused |
| Project, cache, and private task-temp access | Read/write | Read/write |
| Other existing HOME children | Readable; HOME remains unwritable | Readable only when present at launch; new children are denied until the next launch |
| System files | Readable; unwritable | Curated read-only roots; `/proc` is readable, `/sys` is limited, `/run/user`, `/var/run`, `/dev/shm`, `/dev/kmsg`, and shared `/tmp` are omitted |
| Git metadata | Writable so `git add` and `git commit` work | Writable inside project roots |
| Resolved Git hooks, including linked-worktree and `core.hooksPath` locations | Read/write denied after the project allow rule | Read denied; writes inside a writable project remain allowed |
| Nested `.cortexkit` writes | Denied | Not enforceable inside a writable project |
| Unix-domain socket connections such as Docker and SSH agent sockets | Denied by path | Not mediated; connections remain allowed |
| TCP, UDP, DNS, and raw sockets | Open | Open |
| Unsupported native platform | `sandbox_unavailable` | `sandbox_unavailable` |

On Linux, the existing repository-hook read denial requires granting project reads per child present at launch, rather than granting the complete project root. Within one sandboxed command, files newly created at the project root after launch cannot be read, even though they can be written. A subsequent command recomputes the read grants and can read those files. This is a pre-existing Landlock limitation of the nested `.git/hooks` read deny, not a denial of the session's worktree by the CortexKit data floor. macOS does not have this limitation.

### Linux guarantee boundary

The Linux guarantee applies to canonical paths without pre-existing aliases into a granted tree. Granted project, cache, task, and system trees are treated as trusted content. The following limitations are deliberate and surfaced honestly:

- Landlock rules are additive, so nested write-denies under a writable project cannot protect `.git/hooks` or `.cortexkit`. The launcher handles and grants `REFER` only with writable-root rules, which keeps normal in-project renames working and rejects creation of a hard link that would widen access to a denied secret. A pre-existing hard link inside a granted tree remains readable or writable through that alias.
- Landlock does not mediate `AF_UNIX` connects. Docker sockets, `SSH_AUTH_SOCK`, and other pathname Unix sockets can still be reached when normal filesystem permissions allow it.
- Pre-existing bind mounts, case-insensitive filesystem aliases, and overlayfs aliases can expose an object through a granted path. These alias classes are outside the canonical-path guarantee.
- `/proc` is granted wholesale for process and toolchain compatibility. With Yama `ptrace_scope=0`, another same-UID process may expose `/proc/<pid>/environ`, `maps`, or `mem`. Missing, unreadable, or unparseable Yama configuration is treated conservatively as exposed and produces a warning. Yama does not cover every `/proc` surface.

Compared with Codex's default sandbox, AFT is stricter about credential reads: Codex workspace-write can read the host filesystem, including HOME secrets. Codex is stricter about network access and repository metadata: its default disables network access and keeps `.git` read-only, while AFT deliberately leaves the network open and permits Git metadata writes. Neither posture should be described as uniformly stricter.

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

## Durable logs and performance ticks

AFT keeps its own logs under `<storage_root>/logs/`. The storage root follows the
same resolution as indexes and other persistent data: `AFT_STORAGE_DIR`, then
`XDG_DATA_HOME/cortexkit/aft` when set, then the platform data directory (normally
`~/.local/share/cortexkit/aft` on Linux and macOS, or `%LOCALAPPDATA%/cortexkit/aft`
on Windows). See [Uninstall paths](#uninstall-paths) for the complete fallback order.

- Rust module processes write `aft-<pid>.log`. Each process file rolls at 20 MB
  through `.1` to `.5`; files from dead PIDs are removed after seven days.
- OpenCode and Pi plugin messages share `aft-plugin.log`, which uses the same
  20 MB / five-generation rotation policy. The `[aft-plugin]` and `[aft-pi]`
  tags identify the source.
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
