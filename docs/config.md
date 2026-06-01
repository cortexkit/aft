# Configuration

AFT uses a two-level config system: user-level defaults plus project-level overrides.
Both files are JSONC (comments allowed). Config paths are harness-specific:

**OpenCode**

| Scope | Path |
|---|---|
| User | `~/.config/opencode/aft.jsonc` |
| Project | `<project>/.opencode/aft.jsonc` |

**Pi**

| Scope | Path |
|---|---|
| User | `~/.pi/agent/aft.jsonc` |
| Project | `<project>/.pi/aft.jsonc` |

The schema is identical across harnesses. Only file location differs.

## Config Options

```jsonc
{
  // Replace the host harness's built-in tools (read/write/edit/apply_patch/grep/etc.)
  // with AFT-enhanced versions. Default: true. Set to false to use aft_ prefix on all
  // tools instead — useful when you want to keep the harness defaults and access AFT
  // tools alongside them under explicit names.
  "hoist_builtin_tools": true,

  // Auto-format files after every edit. Default: true
  "format_on_edit": true,

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
  // all:         recommended + aft_callgraph, aft_delete, aft_move, aft_transform, aft_refactor
  "tool_surface": "recommended",

  // List of tool names to disable after surface filtering
  "disabled_tools": [],

  // Trigram-indexed grep/glob (graduated from experimental in v0.18).
  // Builds a background index on session start, persists to disk, updates via file watcher.
  // Falls back to direct scanning when the index isn't ready or for out-of-project paths.
  // Default: false
  "search_index": false,

  // Semantic code search (graduated from experimental in v0.18; aft_search tool).
  // Default backend is fastembed (local ONNX, no network) and requires ONNX Runtime
  // installed (brew install onnxruntime on macOS). The model is downloaded on first
  // use. Index persists to disk for fast cold start. To use a remote provider
  // (OpenAI-compatible) or self-hosted Ollama instead, see the "semantic" block
  // below and the aft_search "Embedding backends" section above.
  // Default: false
  "semantic_search": false,

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
    "timeout_ms": 25000,                // per-request timeout, kept under bridge limit
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

  // Maximum source files allowed for call-graph operations (callers, trace_to,
  // trace_data, impact). Projects above this size return "project_too_large"
  // with guidance to open a specific subdirectory. Does not affect grep,
  // glob, read, edit, or any other tool.
  // Default: 5000. Measured cost: ~1ms per source file for the reverse-index
  // build, so 5000 ≈ 5–10s on cold start. The previous 20000 default exceeded
  // the bridge timeout on real ~7K-file projects, surfacing as bridge restart
  // instead of `project_too_large`. Raise this if you have patience and want
  // call-graph navigation on bigger projects.
  "max_callgraph_files": 5000,

  // Request timeout in milliseconds passed to each bridge. The `aft` binary
  // can take 60-120s to start on slow filesystems (WSL2 / DrvFs / network
  // mounts); the previous 30000 default caused immediate configure timeouts
  // followed by repeated bridge respawns ("works, drops, comes back" pattern).
  // Default: 120000. USER-only. Must be a positive integer.
  "configure_timeout_ms": 120000,

  // Number of consecutive silent request timeouts tolerated before tearing
  // down and respawning the bridge. Higher values are safer on slow
  // filesystems; lower values recycle stale bridges faster. Default: 10.
  // USER-only. Must be a positive integer.
  "hang_timeout_threshold": 10,

  // Language servers used for post-edit diagnostics.
  //
  // Built-in servers (auto-registered when their binary is on PATH):
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
    "foreground_wait_window_ms": 8000
  },

  // aft_inspect codebase-health scanner (recommended/all tiers).
  "inspect": {
    "enabled": true,              // set false to drop the aft_inspect tool
    "tier2_idle_minutes": 5       // debounce before idle-triggered Tier 2 background scans
  },

  "experimental": {
    // Use the experimental Astral `ty` Python type checker.
    // Implied when `lsp.python === "ty"`.
    "lsp_ty": false
  }
}
```

AFT auto-detects the formatter and checker from project config files (`biome.json` → biome,
`.oxfmtrc.json` / `.oxfmtrc.jsonc` / `oxfmt.config.ts` → oxfmt, `.prettierrc` → prettier,
`Cargo.toml` → rustfmt, `pyproject.toml` → ruff/black, `go.mod` → goimports). Local tool binaries
(biome, oxfmt, prettier, tsc, pyright) are discovered in
`node_modules/.bin` before falling back to the system PATH. You only need per-language overrides
if auto-detection picks the wrong tool or you want to pin a specific formatter.

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
only if their binary is on `PATH`.

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
which does both automatically).

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
`lsp.disabled` are **user-only** — values from project config (`.opencode/aft.jsonc` or
`.pi/aft.jsonc`) are stripped on load. A hostile repository cannot weaken your supply-chain
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

## Working with large repositories

If you point AFT at a very large directory (monorepo root, `~/Work`, `/home`, etc.), certain
features guard against unbounded work to keep the bridge responsive:

- **Call-graph ops** (`callers`, `trace_to`, `trace_data`, `impact`) return `project_too_large`
  above `max_callgraph_files` (default 5,000 — the empirical limit before the reverse-index build
  exceeds the bridge timeout on real workloads). Raise it in your config if you have patience.
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
