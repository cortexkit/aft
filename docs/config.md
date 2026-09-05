# Configuration

AFT uses a two-level config system: user-level defaults plus project-level overrides.
Both files are JSONC (comments allowed). One location serves every harness:

| Scope | Path |
|---|---|
| User | `~/.config/cortexkit/aft.jsonc` |
| Project | `<project>/.cortexkit/aft.jsonc` |

For the removal order and harness-specific registration steps, see [Uninstall](../README.md#uninstall).

`bash.watch_sync_max_ms` bounds synchronous `bash_watch` calls, which should only cover a short remaining wait on a task; it defaults to 120 seconds because longer synchronous waits keep the agent turn occupied. For longer commands, use `bash({background:true})` and let the completion reminder wake you, or use `bash({wait:true})` when the result is needed before anything else. Values are clamped to 1000..=1800000 with a warning; set it to `1800000` in user or project config to restore the old 30-minute cap.

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

For example, keep OpenCode's built-ins hoisted while Pi exposes both its native tools and the
`aft_*` alternatives:

```jsonc
{
  "harnesses": {
    "opencode": {
      "hoist_builtin_tools": true
    },
    "pi": {
      "hoist_builtin_tools": false
    }
  }
}
```

## Storage Root Environment Override

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
  // Master switch. Default: true. Set false in user config to disable AFT
  // everywhere, or in project config to disable only that project. Project
  // config can set true to re-enable over a user-level false.
  "enabled": true,

   // Edit/read surface: "default" (default) or "hashline". User and project
   // tiers both accept this key; ordinary project-over-user precedence applies.
   "edit_mode": "default",

   // Replace the host harness's native tools with AFT-enhanced versions. Default: true.
   // Set false to keep host-native tools and register AFT replacements under aft_ names
   // (for example, aft_read and aft_bash). The bash companion tools remain unprefixed
   // because they control AFT-owned background task IDs.
   "hoist_builtin_tools": true,

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

  // Tool surface level: "minimal" | "recommended" (default) | "all"
  // minimal:     aft_outline, aft_zoom, aft_safety only (no hoisting)
  // recommended: minimal + hoisted tools (read/write/edit/apply_patch/bash)
  //              + lsp_diagnostics + ast_grep + aft_import + aft_conflicts
  //              + aft_inspect + grep/glob (when search_index is enabled)
  //              + aft_search (when semantic_search is enabled)
  //              (bash sub-features are gated by the top-level `bash` block)
  // all:         recommended + aft_callgraph, aft_delete, aft_move
  "tool_surface": "recommended",

  // List of tool names to disable after surface filtering
  "disabled_tools": [],

  // Trigram-indexed grep/glob (graduated from experimental in v0.18).
  // Builds a background index on session start, persists to disk, updates via file watcher.
  // Falls back to direct scanning when the index isn't ready or for out-of-project paths.
  // Default: false
  "search_index": false,

  // Linked-worktree RAM overlay for the trigram index. Default: false.
  // When true, a borrow-only worktree applies its own file-watcher events to
  // the in-RAM delta of the borrowed search index (and invalidates the symbol
  // cache) so grep/search see local edits. RAM cost scales with the number of
  // changed files. Never writes the shared on-disk cache. Semantic search and
  // the callgraph stay frozen. User and project tiers may both set this.
  "worktree": {
    "ram_overlay": false
  },

  // Semantic code search (graduated from experimental in v0.18; aft_search tool).
  // Default backend is fastembed (local ONNX, no network) and requires ONNX Runtime
  // installed (brew install onnxruntime on macOS). The model is downloaded on first
  // use. Index persists to disk for fast cold start. To use a remote provider
  // (OpenAI-compatible) or self-hosted Ollama instead, see the "semantic" block
  // below and the aft_search "Embedding backends" section above.
  // Default: false
  "semantic_search": false,

  // Content-addressed index views. When enabled, semantic and callgraph artifacts
  // are assembled from reusable per-file blobs behind an atomic manifest.
  // User and project tiers may both set this. Default: false.
  "views": {
    "enabled": false
  },

  // When project_root is exactly $HOME, search_index, semantic_search, and callgraph_store
  // are force-disabled because the home directory is not treated as a project root.

  // Optional embedding-backend configuration for aft_search. Omit this block to use
  // the local fastembed default. Three backends are supported: "fastembed" (default,
  // local ONNX), "openai_compatible" (any /v1/embeddings endpoint — OpenAI, Together,
  // Voyage, vLLM, LM Studio, etc.), and "ollama" (self-hosted at /api/embeddings).
  //
  // USER-only fields: "backend", "base_url", "api_key_env" (project config cannot
  // inject these — strict-allowlist trust boundary). Project config can still tune
  // "model", "timeout_ms", "max_batch_size", "max_files".
  //
  // Switching "backend", "model", or "base_url" deletes the persisted index and
  // rebuilds from scratch on next session start (necessary because dimensions and
  // semantic spaces differ across models). Rotating an API key without changing
  // "api_key_env" does NOT trigger a rebuild.
  "semantic": {
    "backend": "fastembed",            // "fastembed" | "openai_compatible" | "ollama"
    "model": "all-MiniLM-L6-v2",       // model id understood by the backend
    // "base_url": "https://api.openai.com/v1",   // required for openai_compatible / ollama
    // "api_key_env": "OPENAI_API_KEY",            // env var name (not the key itself)
    "timeout_ms": 25000,                // per-request timeout for INDEX BUILDS, kept under bridge limit
    "query_timeout_ms": 3000,           // per-request timeout for interactive QUERY embeds (500-15000).
                                        // Raise for slow providers; on timeout, search degrades to
                                        // lexical for that query instead of failing.
    "max_batch_size": 64,               // embeddings batched in groups of this size
    "max_files": 20000                  // max files indexed (default 20000); raise for remote backends
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
  // @cortexkit/aft-opencode versions automatically when you have @latest in your
  // OpenCode config.plugin entry. When false, the hook still notifies you that an
  // update is available but does not install it. Local-dev (file://) and pinned
  // (@x.y.z) installs always notify-only regardless of this setting.
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

  // Bash hoisting and sub-features (graduated from experimental.bash.* in v0.27.2).
  // Setting any sub-feature true also registers the hoisted `bash` tool plus
  // `bash_status`, `bash_kill`, `bash_watch`, and `bash_write`.
  "bash": {
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

    // Allow subagents to run background bash. Default false — subagent
    // `background: true` requests are otherwise converted to foreground.
    "subagent_background": false,

    // How long a foreground bash call blocks before auto-promoting the task
    // to the background. Minimum 5000; lower values are clamped up. Default 8000.
    "foreground_wait_window_ms": 8000,

    // Pi-only fallback for older Pi versions that cannot report whether its
    // optional default PowerShell tool is enabled. OpenCode never registers it.
    "powershell_tool": false,

    // Whether a new user message detaches a blocking `wait: true` bash call to
    // the background. Default true. Set false to keep the call blocking through
    // steering messages; even then, a message containing `&detach` forces the
    // detach (the token is stripped before the model sees the message).
    "detach_on_user_message": true,

    // Maximum time a synchronous bash_watch call may wait. Defaults to 120000ms;
    // values outside 1000..=1800000 are clamped with a warning. Sync waits are
    // intended for a short remaining wait; to restore the old 30-minute cap,
    // set this to 1800000 in the user or project config.
    "watch_sync_max_ms": 120000
  },

  // aft_inspect codebase-health scanner (recommended/all tiers).
  "inspect": {
    "enabled": true,              // set false to drop the aft_inspect tool
    // Blocking LSP diagnostics deadline. Default 120000; values clamp to
    // 10000..600000. User config sets the baseline; project config may raise
    // it but cannot lower it, so a repository cannot silently reduce another
    // consumer's diagnostic completeness.
    "diagnostics_timeout_ms": 120000,
    "tier2_idle_minutes": 5,      // debounce before idle-triggered Tier 2 background scans
    "duplicates": {
      // Intentional mirror pairs, matched against project-root-relative
      // forward-slash paths. Groups fully spanning one pair are suppressed but
      // still counted in the duplicates summary.
      "expected_mirrors": [["plugin/**", "pi-plugin/**"]]
    }
  },

  // Idle reclamation. User and project tiers. Values outside the documented
  // ranges are clamped with a warning; non-integers are dropped with a warning.
  // Reclaimed indexes rebuild and language servers respawn on the next request.
  "idle": {
    // Minutes without tool traffic before an unbound root's artifacts are
    // evicted. Default 30; clamped to 5..=30.
    "root_ttl_minutes": 30,
    // Minutes without a request before language servers for a root shut down,
    // even while the root is still bound. Default 10; clamped to 1..=10.
    // Independent of root_ttl_minutes.
    "lsp_ttl_minutes": 10
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

  "experimental": {
    // Use the experimental Astral `ty` Python type checker.
    // Implied when `lsp.python === "ty"`.
    "lsp_ty": false
  },

  // Operator hard-off for the `gh` routing shim. Default: true. When false, the
  // shim short-circuits to byte-transparent passthrough (R1) before any
  // daemon/catalog probing, so a disabled shim performs zero subc traffic. This
  // is a fleet-rollout safety gate, not a capability switch. USER-only — a
  // project config cannot disable the shim for the user's host.
  "gh_shim": {
    "enabled": true,
    // Optional absolute development/deployed AFT image. Defaults to the running image.
    "binary_path": "/absolute/path/to/aft"
  },

  // Git co-authorship for commits made by AFT-spawned agent children.
  // "off" (default) | "auto" | an explicit "Name <email>" identity.
  // User and project tiers are accepted; normal project-over-user precedence applies.
    "git": {
      "co_author": "off"
    }
  }
```

On Pi versions that expose the live default-tool registry, AFT hoists `powershell` only when Pi has enabled its optional built-in tool. If that registry is unavailable, set `bash.powershell_tool` to `true` to mirror Pi's setting explicitly. The default is `false`; this key does not register a tool on OpenCode.

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

Hashline mode needs the host's unprefixed `edit` slot. If final surface selection, hoisting, or `disabled_tools` removes that slot, AFT keeps the default edit/read behavior for the session and emits a `hashline_downgraded` warning with reason `edit_not_registered` on the configure-warnings channel.

## `gh` routing shim

AFT maintains `<storage_root>/shims/gh` (or `gh.cmd` on Windows) and prepends that directory only to first-party bash and PTY child processes. The entry dispatches to the running AFT image by default; `gh_shim.binary_path` can select an absolute development or deployed image. The shim routes governed invocations through the daemon seam and passes eligible commands to the first real `gh` later on `PATH`. Set `gh_shim.enabled` to `false` in user config to remove the managed entry and skip child `PATH` injection entirely. The operator's shell startup files and terminal `PATH` are never changed.

## GitHub resource reads

Structured `issue://` and `pr://` reads, concise `aft_outline` indexes, and ordinal `aft_zoom` drill-downs are disabled by default. Set `gh_read.enabled` to `true` in `aft.jsonc` to allow the GitHub read integration to fetch a resource through the user's own `gh` authentication:

```jsonc
{
  "gh_read": {
    "enabled": true
  }
}
```

This is a user-tier-only, host-wide gate; project `gh_read` blocks are dropped with a configuration warning. A project cannot vary the behavior or read-tool surface because project-specific descriptions would destabilize prompt-prefix caches within one host. While the gate is off, the read description remains byte-identical to the baseline surface and does not advertise resource spellings that would only return a refusal. The setting does not affect the `gh_shim` child-process routing gate.

Every enabled GitHub resource read fetches live data; a prior read never satisfies a later request by itself. Successful live renders are retained only as a fallback copy, scoped to the resolved resource and authentication identity. If a live fetch fails and that copy exists, AFT returns it with this exact first-line disclosure before the rendered document:

```text
[cached copy from <ISO8601 UTC>; live fetch failed: <short reason>]
```

If no fallback copy exists, the live-fetch error is returned unchanged. Successful structured `gh` mutations invalidate matching fallback copies, and concurrent reads of the same resource share one in-flight live fetch.

## Git co-authorship

`git.co_author` controls commit attribution for AFT-spawned agent children. `"off"` is the default, `"auto"` derives the repository's bound agent from the gh-shim manifest and cached GitHub numeric ID, and an explicit `"Name <email>"` value is used verbatim. When enabled, AFT selects a complete dispatcher set under `<storage_root>/git-hooks` through child-only `GIT_CONFIG_*` variables; it does not edit global or repository Git configuration. Each dispatcher preserves arguments, stdin, and exit status while chaining to the first executable repository hook from local `core.hooksPath`, the repository's Git directory, or `.githooks`. The `prepare-commit-msg` dispatcher adds attribution first so the repository hook can validate or amend it. AFT quarantines unknown or modified entries in its managed directory and regenerates the expected dispatchers before child launch. Project config may override this attribution key because attribution is not a trust boundary.

## Native command sandbox

Set `sandbox.enabled` to route first-party bash and PTY commands through Seatbelt on macOS or Landlock on Linux. Unsupported platforms, unavailable kernels, Landlock ABIs below V3, invalid profiles, and policies that cannot preserve the credential floor fail closed with a structured `sandbox_unavailable` response. Sandboxed commands receive a private task temporary directory through `TMPDIR`, `TMP`, and `TEMP`; Linux does not grant the shared `/tmp` tree.

The mandatory credential floor is `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.azure`, `~/.config/gcloud`, and `~/.config/cortexkit`. Linux canonicalizes these paths and constructs a read allowlist that omits them. A writable project, cache, temporary directory, or `write_allow` path that overlaps this floor is refused because Landlock cannot subtract write rights. Ordinary `read_deny` paths inside writable roots are supported: writes remain allowed while read grants are split around the denied path.

| Protection | macOS Seatbelt | Linux Landlock |
| --- | --- | --- |
| Credential floor reads and writes | Denied | Denied by omission; overlapping writable roots are refused |
| Project, task artifact, cache, and private task-temp access | Read/write | Read/write |
| Other existing HOME children | Readable; HOME remains unwritable | Readable only when present at launch; new children are denied until the next launch |
| System files | Readable; unwritable | Curated read-only roots; `/proc` is readable, `/sys` is limited, `/run/user`, `/var/run`, `/dev/shm`, `/dev/kmsg`, and shared `/tmp` are omitted |
| Git metadata | Writable so `git add` and `git commit` work | Writable inside project roots |
| Resolved Git hooks, including linked-worktree and `core.hooksPath` locations | Read/write denied after the project allow rule | Read denied; writes inside a writable project remain allowed |
| Nested `.cortexkit` writes | Denied | Not enforceable inside a writable project |
| Unix-domain socket connections such as Docker and SSH agent sockets | Denied by path | Not mediated; connections remain allowed |
| TCP, UDP, DNS, and raw sockets | Open | Open |
| Unsupported native platform | `sandbox_unavailable` | `sandbox_unavailable` |

### Linux guarantee boundary

The Linux guarantee applies to canonical paths without pre-existing aliases into a granted tree. Granted project, cache, task, and system trees are treated as trusted content. The following limitations are deliberate and surfaced honestly:

- Landlock rules are additive, so nested write-denies under a writable project cannot protect `.git/hooks` or `.cortexkit`. The launcher handles and grants `REFER` only with writable-root rules, which keeps normal in-project renames working and rejects creation of a hard link that would widen access to a denied secret. A pre-existing hard link inside a granted tree remains readable or writable through that alias.
- Landlock does not mediate `AF_UNIX` connects. Docker sockets, `SSH_AUTH_SOCK`, and other pathname Unix sockets can still be reached when normal filesystem permissions allow it.
- Pre-existing bind mounts, case-insensitive filesystem aliases, and overlayfs aliases can expose an object through a granted path. These alias classes are outside the canonical-path guarantee.
- `/proc` is granted wholesale for process and toolchain compatibility. With Yama `ptrace_scope=0`, another same-UID process may expose `/proc/<pid>/environ`, `maps`, or `mem`. Missing, unreadable, or unparseable Yama configuration is treated conservatively as exposed and produces a warning. Yama does not cover every `/proc` surface.

Compared with Codex's default sandbox, AFT is stricter about credential reads: Codex workspace-write can read the host filesystem, including HOME secrets. Codex is stricter about network access and repository metadata: its default disables network access and keeps `.git` read-only, while AFT deliberately leaves the network open and permits Git metadata writes. Neither posture should be described as uniformly stricter.

## Config schema migration

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

are migrated automatically on first load to the v0.18 shape:

```jsonc
{
  "search_index": true,        // graduated
  "semantic_search": true,     // graduated
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

AFT runs language servers in-process for post-edit diagnostics and on-demand `lsp_diagnostics`
calls. Servers are spawned lazily — only when a file matching their extensions is touched, and
only if their binary can be resolved from project `node_modules/.bin`, AFT's managed cache, or
`PATH`. Python-family servers additionally check the selected nested workspace's `.venv` or
`venv` first.

**Built-in servers** (auto-registered, no config needed):

| Server | Languages | Binary |
|---|---|---|
| TypeScript Language Server | `.ts .tsx .js .jsx .mjs .cjs` | `typescript-language-server` |
| Pyright | `.py .pyi` | `pyright-langserver` |
| rust-analyzer | `.rs` | `rust-analyzer` |
| gopls | `.go` | `gopls` |
| bash-language-server | `.sh .bash .zsh` | `bash-language-server` |
| yaml-language-server | `.yaml .yml` | `yaml-language-server` |

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
`typescript`, `python` (Pyright), `rust` (rust-analyzer), `go` (gopls), `bash`,
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
| `initialization_options` | no | Passed to the server's LSP `initialize` request |
| `disabled` | no | Skip this server even though it's registered |

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
`build`, `.venv`, and similar).

AFT also honors an optional **`.aftignore`** file: the same syntax as
`.gitignore`, hierarchical, and working in non-git projects, layered on top of
`.gitignore`. Use it to exclude paths AFT shouldn't index that you can't put in
`.gitignore` — most commonly git submodules. Edits under an `.aftignore`d path
also stop triggering reindexing.

Naming a file explicitly in `grep` (e.g. `path: "captures/log.txt"`) searches it
even when it is gitignored or `.aftignore`d, matching ripgrep — an explicitly
named file is always searched.
