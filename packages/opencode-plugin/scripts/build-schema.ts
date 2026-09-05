#!/usr/bin/env bun
/**
 * Generates JSON Schema for aft.jsonc configuration.
 *
 * One schema covers both OpenCode (`~/.config/opencode/aft.jsonc`,
 * `<project>/.opencode/aft.jsonc`) and Pi (`~/.pi/agent/aft.jsonc`,
 * `<project>/.pi/aft.jsonc`) config files. They share the same surface, with
 * a handful of fields that only apply to OpenCode noted in their descriptions.
 *
 * Source of truth is the Zod schema in `packages/opencode-plugin/src/config.ts`
 * (and the matching TypeScript interfaces in `packages/pi-plugin/src/config.ts`).
 * This file is hand-maintained alongside those schemas — if you add or rename
 * a config field, update both the runtime schema AND this builder.
 *
 * Run: bun packages/opencode-plugin/scripts/build-schema.ts
 * Output: assets/aft.schema.json
 */

import * as path from "node:path";

const SCHEMA_URL = "https://raw.githubusercontent.com/cortexkit/aft/main/assets/aft.schema.json";

function buildSchema(): Record<string, unknown> {
  const formatterEnum = {
    type: "string",
    enum: [
      "biome",
      "oxfmt",
      "prettier",
      "deno",
      "ruff",
      "black",
      "rustfmt",
      "goimports",
      "gofmt",
      "none",
    ],
  };

  const checkerEnum = {
    type: "string",
    enum: ["tsc", "tsgo", "biome", "pyright", "ruff", "cargo", "go", "staticcheck", "none"],
  };

  const lspServerEntry = {
    type: "object",
    properties: {
      extensions: {
        type: "array",
        items: { type: "string", minLength: 1 },
        minItems: 1,
        description:
          "File extensions this server handles (e.g. ['.tf', '.tfvars']). Optional when overriding a built-in server — the built-in's extensions are inherited.",
      },
      binary: {
        type: "string",
        minLength: 1,
        description:
          "LSP binary command (must be on PATH or absolute path). Optional when overriding a built-in server — the built-in's binary is inherited.",
      },
      args: {
        type: "array",
        items: { type: "string" },
        default: [],
        description: "Extra command-line arguments passed to the LSP binary.",
      },
      root_markers: {
        type: "array",
        items: { type: "string", minLength: 1 },
        default: [".git"],
        description:
          "Workspace root marker files. AFT walks up from each opened file looking for any of these.",
      },
      disabled: {
        type: "boolean",
        default: false,
        description: "Disable this server entirely without removing the config block.",
      },
      env: {
        type: "object",
        additionalProperties: { type: "string" },
        description: "Extra environment variables passed to the LSP server child process.",
      },
      initialization_options: {
        description:
          "JSON value passed as `initializationOptions` in the LSP `initialize` request.",
      },
    },
    additionalProperties: false,
  };

  const schema: Record<string, unknown> = {
    $schema: "http://json-schema.org/draft-07/schema#",
    $id: SCHEMA_URL,
    title: "AFT Configuration",
    description:
      "Configuration schema for the @cortexkit/aft-opencode and @cortexkit/aft-pi plugins. Place as aft.jsonc in ~/.config/opencode/, <project>/.opencode/, ~/.pi/agent/, or <project>/.pi/.",
    type: "object",
    properties: {
      $schema: { type: "string" },

      enabled: {
        type: "boolean",
        default: true,
        description:
          "Master switch for AFT. Set false in user config to disable AFT everywhere, or in project config to disable it only for that project. Project config can set this because turning AFT off is trust-safe.",
      },

      edit_mode: {
        type: "string",
        enum: ["default", "hashline"],
        default: "default",
        description:
          "Select the edit/read surface. Hashline mode renders tagged reads and changes edit arguments to exactly { patch }.",
      },

      format_on_edit: {
        type: "boolean",
        default: false,
        description:
          "Auto-format files after edits with the language's configured formatter. Default false: formatting can reflow the file under the agent and stale the next edit's context.",
      },

      formatter_timeout_secs: {
        type: "integer",
        minimum: 1,
        maximum: 600,
        default: 10,
        description:
          'Maximum seconds an external formatter is allowed to run before AFT kills it and reports `format_skipped_reason: "timeout"`. Raise for slow formatters in large projects.',
      },

      validate_on_edit: {
        type: "string",
        enum: ["syntax", "full"],
        description:
          "Auto-validate after edits: 'syntax' (tree-sitter parse check) or 'full' (also runs type checker).",
      },

      formatter: {
        type: "object",
        additionalProperties: formatterEnum,
        description:
          "Per-language formatter overrides keyed by language (e.g. 'typescript', 'python', 'rust', 'go').",
      },

      checker: {
        type: "object",
        additionalProperties: checkerEnum,
        description:
          "Per-language type checker overrides keyed by language (e.g. 'typescript', 'python', 'rust', 'go').",
      },

      configure_warnings_delivery: {
        type: "string",
        enum: ["toast", "log", "chat"],
        default: "toast",
        description:
          "How missing formatter/checker/LSP binary warnings are shown after configure. 'toast' (default) uses a 10s TUI or HTTP toast without adding session chat messages. 'log' writes to the plugin log only. 'chat' uses legacy ignored user messages in the session transcript. Warnings for formatters/checkers are only emitted when format_on_edit is true or a per-language formatter is set; checker warnings require validate_on_edit 'syntax' or 'full' or an explicit checker. There is no top-level 'formatters' key — use format_on_edit, formatter, and checker instead.",
      },

      hoist_builtin_tools: {
        type: "boolean",
        default: true,
        description:
          "Replace the host's native file and shell tools with AFT's Rust implementations. Default true. When false, AFT registers its replacements under aft_ names and leaves host-native tools available.",
      },

      tool_surface: {
        type: "string",
        enum: ["minimal", "recommended", "all"],
        default: "recommended",
        description:
          "Tool surface level. 'minimal' = aft_outline+aft_zoom+aft_safety only. 'recommended' (default) adds hoisted read/write/edit/apply_patch + lsp_diagnostics + ast_grep + aft_import. 'all' adds aft_callgraph, aft_delete, aft_move.",
      },

      disabled_tools: {
        type: "array",
        items: { type: "string" },
        description:
          "Tool names to disable. Hoisted names ('read', 'edit') and aft-prefixed names both work. Applied after tool_surface filtering.",
      },

      restrict_to_project_root: {
        type: "boolean",
        default: false,
        description:
          "Restrict file operations to within project root. When true, write-capable commands reject paths outside project_root. Default: false (matches OpenCode built-in behavior).",
      },

      search_index: {
        type: "boolean",
        default: false,
        description:
          "Enable indexed search (trigram index) for grep and glob hoisting. Builds a per-project index for sub-100ms queries on large repos.",
      },

      semantic_search: {
        type: "boolean",
        default: false,
        description:
          "Enable semantic search via aft_search. Backend defaults to local fastembed; configurable via the `semantic` field.",
      },

      callgraph_store: {
        type: "boolean",
        default: true,
        description: "Enable the persisted callgraph store substrate. Default: true.",
      },

      callgraph_chunk_size: {
        type: "number",
        default: 100,
        description:
          "Number of files to parse in a single batch during callgraph store cold build. Lower values reduce peak memory during cold build; set to 0 to parse all files at once.",
      },

      inspect: {
        type: "object",
        properties: {
          enabled: {
            type: "boolean",
            default: true,
            description:
              "Master switch for the aft_inspect tool. Defaults to true. Set false to hide aft_inspect from the tool surface.",
          },
          diagnostics_timeout_ms: {
            type: "integer",
            minimum: 10000,
            maximum: 600000,
            default: 120000,
            description:
              "Blocking LSP diagnostics deadline in milliseconds. Values are clamped to 10000..600000. User config sets the baseline; project config may only raise the effective deadline so a repository cannot silently reduce diagnostic completeness for another consumer.",
          },
          tier2_idle_minutes: {
            type: "number",
            minimum: 0,
            default: 4,
            description:
              "OpenCode session.idle delay in minutes before Tier 2 inspect prewarm runs. Default: 4.",
          },
          categories: {
            type: "object",
            additionalProperties: { type: "boolean" },
            description:
              "Per-category enable/disable overrides keyed by category id (e.g. { 'dead-code': false, 'todos': true }).",
          },
          tier2_soft_deadline_ms: {
            type: "integer",
            minimum: 1,
            description:
              "Soft deadline for Tier 2 inspect analysis in milliseconds. Analysis may be truncated beyond this.",
          },
          max_drill_down_items: {
            type: "integer",
            minimum: 1,
            maximum: 100,
            description:
              "Maximum number of drill-down items returned per inspect category. Capped at 100.",
          },
          duplicates: {
            type: "object",
            properties: {
              expected_mirrors: {
                type: "array",
                items: {
                  type: "array",
                  items: [
                    { type: "string", minLength: 1 },
                    { type: "string", minLength: 1 },
                  ],
                  additionalItems: false,
                  minItems: 2,
                  maxItems: 2,
                },
                description:
                  "Intentional mirror path pairs for duplicate suppression. Each [globA, globB] pair matches project-root-relative forward-slash paths; groups fully straddling the pair are counted as suppressed instead of reported.",
              },
            },
            additionalProperties: false,
            description: "Duplicate suppression config for the duplicates inspect category.",
          },
        },
        additionalProperties: false,
        description:
          "Codebase health inspection config. Enabled by default; set inspect.enabled=false to hide aft_inspect.",
      },

      idle: {
        type: "object",
        properties: {
          root_ttl_minutes: {
            type: "integer",
            default: 30,
            description:
              "Minutes without tool traffic before an unbound root's indexes are evicted. Default 30; values outside 5..=30 are clamped. Reclaimed state rebuilds on the next request. User and project tiers.",
          },
          lsp_ttl_minutes: {
            type: "integer",
            default: 10,
            description:
              "Minutes without a request before language servers for a root are shut down, even while the root is still bound. Default 10; values outside 1..=10 are clamped. Servers respawn on the next diagnostics request. Independent of root_ttl_minutes. User and project tiers.",
          },
        },
        additionalProperties: false,
        description: "Idle reclamation windows for unbound-root artifacts and language servers.",
      },

      worktree: {
        type: "object",
        properties: {
          ram_overlay: {
            type: "boolean",
            default: false,
            description:
              "When true, a linked worktree applies local file-watcher events to the in-RAM trigram delta (and symbol-cache invalidation) so search reflects edits in that worktree. Default false. Never writes the shared on-disk index. Semantic search and callgraph stay frozen. User and project tiers may both set this; it only spends that machine's RAM.",
          },
        },
        additionalProperties: false,
        description:
          "Linked-worktree RAM overlay for the borrowed trigram index. Default off. A repo may opt its worktrees in at project tier.",
      },

      backup: {
        type: "object",
        properties: {
          enabled: {
            type: "boolean",
            default: true,
            description:
              "Master switch for agent-facing undo backups. User-only; project config is ignored.",
          },
          max_depth: {
            type: "integer",
            minimum: 1,
            default: 20,
            description: "Per-file undo stack depth. User-only; project config is ignored.",
          },
          max_file_size: {
            type: "integer",
            minimum: 0,
            default: 64 * 1024 * 1024,
            description:
              "Maximum existing-file size captured for undo, in bytes. Defaults to 64 MiB. User and project tiers may set it; explicit larger values are honored. Zero disables automatic snapshots. Mutations still proceed when capture is skipped.",
          },
        },
        additionalProperties: false,
      },

      sandbox: {
        type: "object",
        properties: {
          enabled: {
            type: "boolean",
            default: false,
            description:
              "Enable native macOS/Linux containment for first-party bash and PTY processes. User-scoped only; project config cannot enable or disable it.",
          },
          write_allow: {
            type: "array",
            items: { type: "string" },
            default: [],
            description:
              "Additional absolute writable directories. User-scoped only; project config cannot add write access.",
          },
          read_deny: {
            type: "array",
            items: { type: "string" },
            default: [],
            description:
              "Additional absolute paths sandboxed commands may not read. Project config may add deny entries but cannot remove user or built-in entries.",
          },
        },
        additionalProperties: false,
        description:
          "Native sandbox policy for first-party bash commands. Disabled by default during staged rollout.",
      },

      bash: {
        oneOf: [
          {
            type: "boolean",
            description:
              "Shorthand: `true` enables hoisting with rewrite + compress + background all on; `false` disables AFT bash hoisting entirely and keeps the host's native bash.",
          },
          {
            type: "object",
            properties: {
              rewrite: {
                type: "boolean",
                default: true,
                description:
                  "Rewrite common bash commands (cat, grep, find, sed, ls) into AFT tool calls for faster, formatted output.",
              },
              compress: {
                type: "boolean",
                default: true,
                description:
                  "Compress bash output via per-tool compressors (git, cargo, npm, bun, pnpm, pytest, tsc, eslint, biome, vitest, prettier, ruff, mypy, go, golangci-lint, playwright, next) plus TOML filter pipeline. Adds `[cmpaft]` marker.",
              },
              background: {
                type: "boolean",
                default: true,
                description:
                  "Allow agents to launch bash with `{ background: true }` for long-running tasks. Foreground bash always auto-promotes to background after the foreground wait window (default 8s) regardless of this flag.",
              },
              host_fallback: {
                type: "boolean",
                default: false,
                description:
                  "Break-glass host execution when AFT's module transport is unavailable. Project-settable. Every fallback command requires a fresh host permission prompt and runs without AFT rewrites, compression, or background support.",
              },
              subagent_background: {
                type: "boolean",
                default: false,
                description:
                  "Allow subagents to run background bash. Default false — subagent `background: true` requests are otherwise converted to foreground so the subagent turn does not end early.",
              },
              detach_on_user_message: {
                type: "boolean",
                default: true,
                description:
                  "Detach a `wait: true` bash call when a new user message arrives. Default true. Set false to keep the wait blocking; a message containing the literal `&detach` still forces detachment, the token is stripped before delivery and the rest of the message is preserved; a token-only message becomes `(requested background detach)`. Project-safe.",
              },
              watch_sync_max_ms: {
                type: "integer",
                minimum: 1000,
                maximum: 1800000,
                default: 120000,
                description:
                  "Maximum synchronous bash_watch wait in milliseconds. Defaults to 120 seconds for short remaining waits; set to 1800000 to restore the old 30-minute cap.",
              },
              long_running_reminder_enabled: {
                type: "boolean",
                default: true,
                description:
                  "Periodically remind the agent that a background bash task is still running. When false, completion is delivered but mid-flight reminders are suppressed.",
              },
              long_running_reminder_interval_ms: {
                type: "integer",
                minimum: 1,
                default: 600000,
                description:
                  "Interval in milliseconds between mid-flight reminders for a still-running background bash task.",
              },
              foreground_wait_window_ms: {
                type: "integer",
                minimum: 5000,
                default: 8000,
                description:
                  "How long foreground bash blocks before auto-promoting the task to background, in milliseconds. Minimum 5000; values below the floor are clamped up.",
              },
              powershell_tool: {
                type: "boolean",
                default: false,
                description:
                  "Pi-only fallback for manually mirroring Pi's optional PowerShell default tool when the host does not expose its enabled-tool registry. OpenCode never registers this tool.",
              },
            },
            additionalProperties: false,
          },
        ],
        description:
          "Bash tool family (hoist + rewrite + compress + background execution). Default on for `tool_surface: recommended`/`all`, off for `minimal`. Replaces `experimental.bash.*` (still accepted for backward compat).",
      },

      experimental: {
        type: "object",
        properties: {
          bash: {
            type: "object",
            properties: {
              rewrite: {
                type: "boolean",
                default: false,
                description:
                  "Rewrite common bash commands (cat, grep, find, sed, ls) into AFT tool calls for faster, formatted output.",
              },
              compress: {
                type: "boolean",
                default: false,
                description:
                  "Compress bash output via per-tool compressors (git, cargo, npm, bun, pnpm, pytest, tsc, eslint, vitest, biome) plus TOML filter pipeline. Adds `[cmpaft]` marker.",
              },
              background: {
                type: "boolean",
                default: false,
                description:
                  "Allow agents to launch bash with `{ background: true }` for long-running tasks. Foreground bash always auto-promotes to background after 5s regardless of this flag.",
              },
              long_running_reminder_enabled: {
                type: "boolean",
                default: true,
                description:
                  "Periodically remind the agent that a background bash task is still running. When false, completion is delivered but mid-flight reminders are suppressed.",
              },
              long_running_reminder_interval_ms: {
                type: "integer",
                minimum: 1,
                default: 600000,
                description:
                  "Interval in milliseconds between mid-flight reminders for a still-running background bash task.",
              },
            },
            additionalProperties: false,
            description:
              "Experimental bash hoisting / rewrite / compression / background features.",
          },
          lsp_ty: {
            type: "boolean",
            default: false,
            description:
              "Run the experimental Python `ty` type checker alongside Pyright. Use lsp.python = 'ty' to select ty exclusively.",
          },
        },
        additionalProperties: false,
        description: "Experimental opt-in features. May change between releases.",
      },

      lsp: {
        type: "object",
        properties: {
          servers: {
            type: "object",
            additionalProperties: lspServerEntry,
            description:
              "User-defined LSP server map keyed by server id (e.g. { 'terraform-ls': { ... } }).",
          },
          disabled: {
            type: "array",
            items: { type: "string", minLength: 1 },
            description:
              "Built-in LSP server ids to disable (e.g. ['python', 'biome']). See README for the full list.",
          },
          python: {
            type: "string",
            enum: ["pyright", "ty", "auto"],
            default: "auto",
            description:
              "Which Python LSP to use. 'auto' stays on Pyright while ty is experimental; select 'ty' explicitly to opt in without fallback.",
          },
          diagnostics_on_edit: {
            type: "boolean",
            default: false,
            description:
              "Wait for inline LSP diagnostics on every edit/write/apply_patch call. Default: false.",
          },
          auto_install: {
            type: "boolean",
            default: true,
            description:
              "Auto-install npm-distributed and GitHub-release LSP servers when the project needs them. Set false to require manual install on PATH.",
          },
          grace_days: {
            type: "integer",
            minimum: 1,
            default: 7,
            description:
              "Supply-chain grace window. AFT only installs versions that have been on the registry / GitHub releases for at least this many days. User pins via `lsp.versions` bypass this.",
          },
          versions: {
            type: "object",
            additionalProperties: { type: "string", minLength: 1 },
            description:
              "Per-package version pin map keyed by npm package or GitHub repo. Pins bypass the grace filter and any weekly version recheck (e.g. { 'typescript-language-server': '5.0.0', 'clangd/clangd': '21.1.0' }).",
          },
        },
        additionalProperties: false,
        description: "User-defined and built-in LSP server configuration.",
      },

      url_fetch_allow_private: {
        type: "boolean",
        default: false,
        description:
          "Allow `aft_outline`/`aft_zoom` URL fetches to request private/link-local hosts. Default: false (rejects RFC1918, loopback, and link-local).",
      },

      semantic: {
        type: "object",
        properties: {
          backend: {
            type: "string",
            enum: ["fastembed", "openai_compatible", "ollama", "synapse"],
            default: "fastembed",
            description:
              "Embedding backend. 'fastembed' uses local ONNX runtime, 'openai_compatible' calls a configured OpenAI-style API, 'ollama' calls a local Ollama embedding endpoint, and 'synapse' dials CortexKit Synapse over SubC.",
          },
          model: {
            type: "string",
            minLength: 1,
            description:
              "Model identifier passed to the backend. Required for synapse; fastembed defaults to all-MiniLM-L6-v2.",
          },
          base_url: {
            type: "string",
            minLength: 1,
            description:
              "Base URL of the backend API endpoint. Required for openai_compatible. Default for ollama: http://localhost:11434.",
          },
          api_key_env: {
            type: "string",
            minLength: 1,
            description:
              "Environment variable name containing the API key (e.g. 'OPENAI_API_KEY'). Project-scoped configs cannot set this field — only user-scoped configs can.",
          },
          timeout_ms: {
            type: "integer",
            minimum: 1,
            default: 25000,
            description: "Background build embedding request timeout in milliseconds.",
          },
          query_timeout_ms: {
            type: "integer",
            minimum: 500,
            maximum: 15000,
            default: 3000,
            description:
              "Interactive query embedding deadline in milliseconds. Project-scoped configs cannot set this field; values are clamped to 500..15000.",
          },
          max_batch_size: {
            type: "integer",
            minimum: 1,
            description: "Maximum batch size used by the semantic embedding pipeline.",
          },
          max_files: {
            type: "integer",
            minimum: 1,
            default: 20000,
            description:
              "Maximum number of project files to semantically index (default 20000). Guards local fastembed memory on large roots; raise it for remote backends that embed server-side.",
          },
        },
        additionalProperties: false,
        description: "External semantic backend configuration for embedding and retrieval.",
      },

      bridge: {
        type: "object",
        properties: {
          request_timeout_ms: {
            type: "integer",
            minimum: 1000,
            default: 30000,
            description:
              "Per-request bridge transport timeout in milliseconds. Default 30000. Raise on slow filesystems (WSL/DrvFs/NFS) where cold aft operations exceed the default.",
          },
          hang_threshold: {
            type: "integer",
            minimum: 1,
            default: 2,
            description:
              "Consecutive silent request timeouts before the shared bridge process is killed and respawned (aborting all pending requests). Default 2. Raise when many editor windows share one bridge.",
          },
        },
        additionalProperties: false,
        description:
          "Shared NDJSON bridge transport tuning (OpenCode and Pi). User-scoped only — project configs cannot set this block (bridge safety and per-machine transport budget).",
      },

      subc: {
        type: "object",
        properties: {
          connection_file: {
            type: "string",
            description:
              "Absolute path to the Subconscious (subc) daemon connection file. When present (non-empty), the plugin talks to AFT as a daemon-supervised module over subc instead of spawning the aft binary; absent/empty means standalone NDJSON (the default). macOS default: ~/.local/share/cortexkit/run/subc-connection.json.",
          },
        },
        additionalProperties: false,
        description:
          "Subconscious (subc) daemon transport selection. User-scoped only — a project config cannot redirect transport. Presence of connection_file switches AFT from a spawned child process to a daemon-supervised module.",
      },

      gh_shim: {
        type: "object",
        properties: {
          enabled: {
            type: "boolean",
            default: true,
            description:
              "Operator hard-off for child PATH injection and governed gh routing. User-scoped only.",
          },
          binary_path: {
            type: "string",
            description:
              "Absolute deployed or development AFT binary used by the managed gh entry. Defaults to the running AFT image. User-scoped only.",
          },
        },
        additionalProperties: false,
        description:
          "gh routing shim operator gate. User-scoped only — a project config cannot disable the shim for the user's host.",
      },

      gh_read: {
        type: "object",
        properties: {
          enabled: {
            type: "boolean",
            default: false,
            description:
              "Enable structured issue:// and pr:// reads. Default false; user-scoped only because it changes the globally registered read-tool surface.",
          },
        },
        additionalProperties: false,
        description:
          "GitHub resource-read operator gate. User-scoped only — a project config cannot change host-wide behavior or tool descriptions.",
      },

      git: {
        type: "object",
        properties: {
          co_author: {
            type: "string",
            default: "off",
            description:
              "Git co-author attribution for AFT-spawned agent children: 'off', 'auto', or an explicit 'Name <email>' identity. User and project tiers are accepted with project precedence.",
          },
        },
        additionalProperties: false,
        description:
          "Agent-child Git attribution injected through environment-scoped core.hooksPath configuration.",
      },

      auto_update: {
        type: "boolean",
        default: true,
        description:
          "OpenCode only: auto-refresh the cached @cortexkit/aft-opencode package when a newer channel version is published. User-scoped only — project configs cannot disable updates silently.",
      },
    },
    additionalProperties: false,
  };

  const configProperties = schema.properties as Record<string, unknown>;
  configProperties.harnesses = {
    type: "object",
    description:
      "Per-harness overrides. The active harness applies its override after the base config within each tier: user base, user override, project base, project override. Nested harnesses are ignored; unknown harness names are reserved for forward compatibility.",
    additionalProperties: {
      type: "object",
      properties: { ...configProperties },
      additionalProperties: false,
    },
  };
  return schema;
}

async function main() {
  const rootDir = path.resolve(import.meta.dir, "..", "..", "..");
  const assetsDir = path.join(rootDir, "assets");
  const outputPath = path.join(assetsDir, "aft.schema.json");

  const fs = await import("node:fs");
  if (!fs.existsSync(assetsDir)) {
    fs.mkdirSync(assetsDir, { recursive: true });
  }

  const schema = buildSchema();
  await Bun.write(outputPath, `${JSON.stringify(schema, null, 2)}\n`);
  console.log(`✓ JSON Schema generated: ${outputPath}`);
}

main();
