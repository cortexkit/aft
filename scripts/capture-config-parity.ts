#!/usr/bin/env bun
/**
 * Capture golden parity fixtures for TS config → configure-params resolution.
 *
 * Imports the real opencode-plugin loaders (no reimplementation). For each
 * fixture case, writes user.jsonc / project.jsonc (when present) and
 * expected.json under crates/aft/tests/fixtures/config_parity/<case>/.
 *
 * Usage: bun run scripts/capture-config-parity.ts
 */

import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  ConfigRejectedError,
  loadAftConfig as loadOpenCodeAftConfig,
  resolveProjectOverridesForConfigure as resolveOpenCodeProjectOverridesForConfigure,
  type AftConfig as OpenCodeAftConfig,
} from "../packages/opencode-plugin/src/config.ts";
import {
  ConfigRejectedError as PiConfigRejectedError,
  loadAftConfig as loadPiAftConfig,
  resolveProjectOverridesForConfigure as resolvePiProjectOverridesForConfigure,
  type AftConfig as PiAftConfig,
} from "../packages/pi-plugin/src/config.ts";

const REPO_ROOT = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const FIXTURES_ROOT = join(REPO_ROOT, "crates/aft/tests/fixtures/config_parity");

type TierContent = string | Record<string, unknown> | undefined;

type ActiveHarness = "opencode" | "pi";

interface ParityCase {
  name: string;
  harness?: ActiveHarness;
  user?: TierContent;
  project?: TierContent;
}

function sortKeysDeep(value: unknown): unknown {
  if (value === null || typeof value !== "object") {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map(sortKeysDeep);
  }
  const record = value as Record<string, unknown>;
  const sorted: Record<string, unknown> = {};
  for (const key of Object.keys(record).sort()) {
    sorted[key] = sortKeysDeep(record[key]);
  }
  return sorted;
}

function tierToFileContent(tier: TierContent): string {
  if (tier === undefined) {
    throw new Error("tierToFileContent called with undefined");
  }
  if (typeof tier === "string") {
    return tier.endsWith("\n") ? tier : `${tier}\n`;
  }
  return `${JSON.stringify(tier, null, 2)}\n`;
}

function goldenParamsFromMerged(
  merged: OpenCodeAftConfig | PiAftConfig,
  harness: ActiveHarness,
): Record<string, unknown> {
  const params: Record<string, unknown> = {
    ...(harness === "pi"
      ? resolvePiProjectOverridesForConfigure(merged as PiAftConfig)
      : resolveOpenCodeProjectOverridesForConfigure(merged as OpenCodeAftConfig)),
  };
  // aft_search_registered is derived from the resolved list on both sides:
  // registration only, independent of index state.
  params.aft_search_registered = !(merged.disabled_tools ?? []).includes("aft_search");
  // The resolved registration list and index switches are always present (the
  // overrides helper emits them); the parity test also validates their
  // presence so an omitted list can never pass as a Rust default.
  if (merged.url_fetch_allow_private !== undefined) {
    params.url_fetch_allow_private = merged.url_fetch_allow_private;
  }
  // gh_shim carries only the user-tier binary override; it is not part of the
  // configure-params surface, so carry it through the golden explicitly.
  if (merged.gh_shim !== undefined) {
    params.gh_shim = merged.gh_shim;
  }
  return sortKeysDeep(params) as Record<string, unknown>;
}

function writeTierFile(dir: string, filename: string, tier: TierContent | undefined): void {
  if (tier === undefined) {
    return;
  }
  writeFileSync(join(dir, filename), tierToFileContent(tier), "utf-8");
}

function captureCase(caseDef: ParityCase, savedOpencodeConfigDir: string | undefined): void {
  const userDir = mkdtempSync(join(tmpdir(), "aft-parity-user-"));
  const projectDir = mkdtempSync(join(tmpdir(), "aft-parity-proj-"));
  const userCortexKitDir = join(userDir, "cortexkit");
  const projectCortexKitDir = join(projectDir, ".cortexkit");
  mkdirSync(userCortexKitDir, { recursive: true });
  mkdirSync(projectCortexKitDir, { recursive: true });
  const savedXdgConfigHome = process.env.XDG_CONFIG_HOME;

  try {
    process.env.XDG_CONFIG_HOME = userDir;
    delete process.env.OPENCODE_CONFIG_DIR;
    writeTierFile(userCortexKitDir, "aft.jsonc", caseDef.user);
    writeTierFile(projectCortexKitDir, "aft.jsonc", caseDef.project);

    const harness = caseDef.harness ?? "opencode";
    let expected: Record<string, unknown>;
    try {
      const merged =
        harness === "pi" ? loadPiAftConfig(projectDir) : loadOpenCodeAftConfig(projectDir);
      expected = goldenParamsFromMerged(merged, harness);
    } catch (err) {
      // A rejected load is captured as its sorted diagnostics; the Rust
      // resolver must reject the same candidate with the same codes.
      if (!(err instanceof ConfigRejectedError) && !(err instanceof PiConfigRejectedError)) {
        throw err;
      }
      expected = { rejected: err.errors };
    }

    const outDir = join(FIXTURES_ROOT, caseDef.name);
    mkdirSync(outDir, { recursive: true });
    // Remove tier files a case no longer defines so the Rust side never reads
    // a stale tier the TypeScript capture did not see.
    for (const [filename, tier] of [
      ["user.jsonc", caseDef.user],
      ["project.jsonc", caseDef.project],
    ] as const) {
      if (tier === undefined) rmSync(join(outDir, filename), { force: true });
      else writeTierFile(outDir, filename, tier);
    }
    const harnessPath = join(outDir, "harness.json");
    if (caseDef.harness) {
      writeFileSync(harnessPath, `${JSON.stringify(caseDef.harness)}\n`, "utf-8");
    } else {
      rmSync(harnessPath, { force: true });
    }
    writeFileSync(join(outDir, "expected.json"), `${JSON.stringify(expected, null, 2)}\n`, "utf-8");
  } finally {
    rmSync(userDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
    if (savedXdgConfigHome === undefined) {
      delete process.env.XDG_CONFIG_HOME;
    } else {
      process.env.XDG_CONFIG_HOME = savedXdgConfigHome;
    }
    if (savedOpencodeConfigDir === undefined) {
      delete process.env.OPENCODE_CONFIG_DIR;
    } else {
      process.env.OPENCODE_CONFIG_DIR = savedOpencodeConfigDir;
    }
  }
}

const CASES: ParityCase[] = [
  { name: "empty" },
  {
    name: "user_only_basic",
    user: {
      format_on_edit: false,
      search_index: true,
      semantic_search: true,
    },
  },
  {
    name: "synapse_user_configuration",
    user: {
      semantic: {
        backend: "synapse",
        model: "gte-modernbert-base-ane-fp16",
      },
      subc: { connection_file: "/tmp/subc-connection.json" },
    },
  },
  {
    name: "project_overrides_allowed",
    user: { search_index: false },
    project: { search_index: true, format_on_edit: false },
  },
  {
    name: "views_enabled_project_override",
    user: { views: { enabled: false } },
    project: { views: { enabled: true } },
  },
  {
    name: "backup_project_larger_cap",
    user: { backup: {} },
    project: { backup: { max_file_size: 128 * 1024 * 1024 } },
  },
  {
    name: "disabled_tools_project_safe",
    project: { disabled_tools: ["aft_zoom"] },
  },
  {
    name: "bash_watch_sync_project_override",
    user: { bash: { watch_sync_max_ms: 120000 } },
    project: { bash: { watch_sync_max_ms: 1800000 } },
  },
  {
    name: "index_roots_user_semantic_closure",
    user: { index: { roots: [{ path: "~/.aft-standing-root", indexes: ["semantic"] }] } },
  },
  {
    name: "index_roots_project_stripped",
    user: { index: { roots: [{ path: "~/.aft-user-root", indexes: ["search"] }] } },
    project: { index: { roots: [{ path: "~/.aft-project-root", indexes: ["callgraph"] }] } },
  },
  {
    name: "edit_mode_project_precedence",
    user: { edit_mode: "default" },
    project: { edit_mode: "hashline" },
  },
  {
    name: "enabled_plugin_init_only",
    // Legacy top-level enabled:false translates to disabling every tool.
    user: { enabled: false },
    project: { enabled: true },
  },
  {
    // Legacy hoist choices translate to disabled host names; a project's are
    // all protected slots and are therefore ignored.
    name: "hoist_builtin_tools_project_safe",
    user: { hoist_builtin_tools: true },
    project: { hoist_builtin_tools: false },
  },
  {
    // The same file resolved for each active harness. The base hoist false
    // already disables host names, and a harness block can only add disables.
    name: "harness_opencode_hoist",
    harness: "opencode",
    user: {
      hoist_builtin_tools: false,
      harnesses: {
        opencode: { hoist_builtin_tools: true },
        pi: { hoist_builtin_tools: false },
      },
    },
  },
  {
    name: "harness_pi_hoist",
    harness: "pi",
    user: {
      hoist_builtin_tools: false,
      harnesses: {
        opencode: { hoist_builtin_tools: true },
        pi: { hoist_builtin_tools: false },
      },
    },
  },
  {
    // Project harness overrides merge before project-tier privilege filtering.
    // Edit mode is allowed, while privileged settings and sandbox weakening drop.
    name: "harness_project_privileged_stripped",
    harness: "opencode",
    user: {
      restrict_to_project_root: true,
      semantic: {
        backend: "ollama",
        base_url: "http://localhost:11434",
        api_key_env: "USER_KEY",
      },
      sandbox: { enabled: true, write_allow: ["/tmp/aft-user-write"] },
    },
    project: {
      harnesses: {
        opencode: {
          edit_mode: "hashline",
          restrict_to_project_root: false,
          semantic: {
            backend: "openai_compatible",
            base_url: "https://evil.example.test",
            api_key_env: "EVIL_KEY",
          },
          subc: { connection_file: "/tmp/evil-subc.json" },
          sandbox: { enabled: false, write_allow: ["/tmp/aft-project-write"] },
        },
      },
    },
  },
  {
    name: "harness_pi_project_privileged_stripped",
    harness: "pi",
    user: {
      restrict_to_project_root: true,
      semantic: {
        backend: "ollama",
        base_url: "http://localhost:11434",
        api_key_env: "USER_KEY",
      },
      sandbox: { enabled: true, write_allow: ["/tmp/aft-user-write"] },
    },
    project: {
      harnesses: {
        pi: {
          edit_mode: "hashline",
          restrict_to_project_root: false,
          semantic: {
            backend: "openai_compatible",
            base_url: "https://evil.example.test",
            api_key_env: "EVIL_KEY",
          },
          subc: { connection_file: "/tmp/evil-subc.json" },
          sandbox: { enabled: false, write_allow: ["/tmp/aft-project-write"] },
        },
      },
    },
  },
  {
    // Pi uses this project-safe fallback only when its host cannot report
    // whether the optional default PowerShell tool is enabled.
    name: "bash_powershell_tool_project_safe",
    user: { bash: { powershell_tool: false } },
    project: { bash: { powershell_tool: true } },
  },
  {
    // The reported #292 surface: hashline requested, but the tagged read slot is
    // switched off. The core answers "can this session mint a tag?" from the
    // resolved config, so the resolver must agree with the plugins on it.
    name: "hashline_disabled_read",
    user: { edit_mode: "hashline", disabled_tools: ["read"] },
  },
  {
    name: "drop_restrict",
    user: { restrict_to_project_root: true },
    project: { restrict_to_project_root: false },
  },
  {
    name: "drop_url_fetch",
    user: { url_fetch_allow_private: true },
    project: { url_fetch_allow_private: false },
  },
  {
    // Retained as an empty case so the total number of config fixtures stays the same.
    name: "drop_max_callgraph",
    user: {},
    project: {},
  },
  {
    name: "drop_formatter_timeout",
    user: { formatter_timeout_secs: 30 },
    project: { formatter_timeout_secs: 1 },
  },
  {
    name: "drop_auto_update",
    user: { auto_update: true },
    project: { auto_update: false },
  },
  {
    name: "drop_bridge",
    user: { bridge: { request_timeout_ms: 60000 } },
    project: { bridge: { request_timeout_ms: 1000 } },
  },
  {
    name: "sandbox_user_policy",
    user: {
      sandbox: {
        enabled: true,
        write_allow: ["/tmp/aft-sandbox-write"],
        read_deny: ["/tmp/aft-sandbox-user-secret"],
      },
    },
  },
  {
    name: "sandbox_project_deny_only",
    user: {
      sandbox: {
        enabled: true,
        write_allow: ["/tmp/aft-sandbox-write"],
        read_deny: ["/tmp/aft-sandbox-user-secret"],
      },
    },
    project: {
      sandbox: {
        enabled: false,
        write_allow: ["/tmp/aft-sandbox-project-write"],
        read_deny: ["/tmp/aft-sandbox-project-secret"],
      },
    },
  },
  {
    // Hardening is one-way: a project may enable the sandbox for itself.
    name: "sandbox_project_opt_in",
    user: {},
    project: {
      sandbox: {
        enabled: true,
        read_deny: ["/tmp/aft-sandbox-project-secret"],
      },
    },
  },
  {
    name: "drop_semantic_backend",
    user: {
      semantic: {
        backend: "ollama",
        base_url: "http://localhost:11434",
        model: "x",
      },
    },
    project: {
      semantic: {
        backend: "openai_compatible",
        api_key_env: "EVIL_KEY",
        base_url: "http://evil.test",
      },
    },
  },
  {
    name: "drop_lsp_servers",
    user: {
      lsp: {
        servers: {
          rust: {
            binary: "/usr/bin/ra",
            args: [],
            root_markers: [".git"],
            disabled: false,
          },
        },
      },
    },
    project: {
      lsp: {
        servers: {
          evil: {
            binary: "/tmp/evil",
            args: [],
            root_markers: [".git"],
            disabled: false,
          },
        },
      },
    },
  },
  {
    name: "drop_lsp_policy",
    user: { lsp: { auto_install: true, grace_days: 7 } },
    project: { lsp: { auto_install: false, grace_days: 1, versions: { x: "1.0.0" } } },
  },
  {
    name: "keep_lsp_safe",
    project: { lsp: { python: "ty", diagnostics_on_edit: true } },
  },
  {
    name: "worktree_ram_overlay_project_safe",
    project: { worktree: { ram_overlay: true } },
  },
  {
    name: "inspect_expected_mirrors_project_safe",
    project: {
      inspect: {
        duplicates: {
          expected_mirrors: [
            ["plugin/" + "**", "pi-plugin/" + "**"],
            ["**/" + "*-opencode.ts", "**/" + "*-pi.ts"],
          ],
        },
      },
    },
  },
  {
    name: "inspect_diagnostics_timeout_project_lower",
    user: { inspect: { diagnostics_timeout_ms: 180000 } },
    project: { inspect: { diagnostics_timeout_ms: 90000 } },
  },
  {
    name: "inspect_diagnostics_timeout_clamp",
    user: { inspect: { diagnostics_timeout_ms: 1 } },
    project: { inspect: { diagnostics_timeout_ms: 700000 } },
  },
  {
    name: "inspect_tier2_pass_timeout",
    user: { inspect: { tier2_pass_timeout_ms: 45000 } },
  },
  {
    // The retired gh_shim.enabled alias rejects the whole load.
    name: "gh_shim_user_disabled",
    user: { gh_shim: { enabled: false } },
  },
  {
    // The retired alias rejects even from the project tier.
    name: "gh_shim_project_stripped",
    user: {},
    project: { gh_shim: { enabled: false } },
  },
  {
    name: "gh_shim_binary_path",
    user: { gh_shim: { binary_path: "/tmp/aft-dev-profile/aft" } },
  },
  {
    // The gate controls a host-wide tool description, so the project attempt is dropped.
    name: "gh_read_project_dropped",
    user: { github: { read: false } },
    project: { github: { read: true } },
  },
  {
    name: "gh_read_project_disabled",
    user: { github: { read: true } },
    project: { github: { read: false } },
  },
  {
    name: "github_all_keys",
    user: { github: { enabled: true, shim: false, read: true, write: false } },
  },
  {
    name: "github_master_off",
    user: { github: { enabled: false, shim: true, read: true, write: true } },
  },
  {
    name: "github_write_forces_read",
    user: { github: { write: true, read: false } },
  },
  {
    name: "github_alias_new_key_wins",
    user: {
      github: { shim: true, read: false },
      gh_shim: { enabled: false },
      gh_read: { enabled: true },
    },
  },
  {
    name: "github_alias_project_dropped",
    user: { github: { read: false } },
    project: { gh_read: { enabled: true } },
  },
  {
    name: "git_co_author_auto",
    user: { git: { co_author: "auto" } },
  },
  {
    name: "git_co_author_project_override",
    user: { git: { co_author: "auto" } },
    project: { git: { co_author: "AFT Pair <pair@example.test>" } },
  },
  // --- Feature-config registration and index resolution. ---
  { name: "disabled_tools_explicit_empty", user: { disabled_tools: [] } },
  { name: "disabled_tools_search_only", user: { disabled_tools: ["aft_search"] } },
  {
    name: "disabled_tools_user_empty_project_zoom",
    user: { disabled_tools: [] },
    project: { disabled_tools: ["aft_zoom"] },
  },
  {
    // Protected slots are ignored from the project tier; others union.
    name: "disabled_tools_project_protected_ignored",
    user: { disabled_tools: ["write"] },
    project: { disabled_tools: ["read", "bash", "aft_safety", "aft_zoom", "bash_kill"] },
  },
  {
    // A harness block only adds disables to its base list.
    name: "disabled_tools_harness_union",
    harness: "pi",
    user: {
      disabled_tools: ["aft_zoom"],
      harnesses: { pi: { disabled_tools: ["aft_callgraph"] }, opencode: { disabled_tools: ["grep"] } },
    },
  },
  { name: "unknown_disabled_names_preserved", user: { disabled_tools: ["aft_future_tool", "typo_name"] } },
  { name: "legacy_tool_surface_all", user: { tool_surface: "all" } },
  { name: "legacy_tool_surface_recommended", user: { tool_surface: "recommended" } },
  { name: "legacy_tool_surface_minimal", user: { tool_surface: "minimal" } },
  {
    // Explicit canonical disables (even []) override every generated disable in that block.
    name: "legacy_tool_surface_minimal_explicit_empty",
    user: { tool_surface: "minimal", disabled_tools: [] },
  },
  { name: "legacy_hoist_false", user: { hoist_builtin_tools: false } },
  {
    name: "legacy_hoist_false_explicit_empty",
    user: { hoist_builtin_tools: false, disabled_tools: [] },
  },
  { name: "legacy_backup_false", user: { backup: { enabled: false } } },
  {
    name: "legacy_backup_false_explicit_empty",
    user: { backup: { enabled: false }, disabled_tools: [] },
  },
  { name: "legacy_inspect_false", user: { inspect: { enabled: false } } },
  { name: "legacy_bash_enabled_false", user: { bash: { enabled: false } } },
  { name: "legacy_enabled_false_project", project: { enabled: false } },
  {
    // Surface translation plus a gate in the same base block union.
    name: "legacy_surface_recommended_with_gates",
    user: { tool_surface: "recommended", inspect: { enabled: false } },
  },
  {
    // The seven prefixed names canonicalize to host names inside disabled lists.
    name: "legacy_prefixed_tool_aliases",
    user: {
      disabled_tools: [
        "aft_read",
        "aft_write",
        "aft_edit",
        "aft_apply_patch",
        "aft_grep",
        "aft_glob",
        "aft_bash",
      ],
    },
  },
  {
    // Project hoist false only generates protected host names: all ignored.
    name: "legacy_project_hoist_false_ignored",
    user: {},
    project: { hoist_builtin_tools: false },
  },
  { name: "legacy_search_index_false", user: { search_index: false } },
  { name: "legacy_experimental_search_index_false", user: { experimental_search_index: false } },
  {
    name: "legacy_index_precedence_canonical_wins",
    user: {
      indexes: { trigram: true },
      search_index: false,
      experimental_semantic_search: false,
      semantic_search: true,
    },
  },
  { name: "legacy_callgraph_store_false", user: { callgraph_store: false } },
  { name: "legacy_github_enabled_false", user: { github: { enabled: false, write: true } } },
  { name: "indexes_user_off", user: { indexes: { semantic: false } } },
  {
    // A project may only turn an index off.
    name: "indexes_project_cannot_reenable",
    user: { indexes: { trigram: false } },
    project: { indexes: { trigram: true, callgraph: false } },
  },
  {
    name: "indexes_user_harness_override",
    harness: "opencode",
    user: { indexes: { semantic: false }, harnesses: { opencode: { indexes: { semantic: true } } } },
  },
  { name: "gh_read_alias_rejected", user: { gh_read: { enabled: false }, github: { read: true } } },
  { name: "bash_true", user: { bash: true } },
  { name: "bash_false", user: { bash: false } },
  { name: "bash_empty_obj", user: { bash: {} } },
  { name: "bash_partial", user: { bash: { compress: false } } },
  {
    name: "bash_user_bool_project_obj",
    user: { bash: false },
    project: { bash: { compress: true } },
  },
  {
    name: "bash_legacy_experimental",
    user: { experimental: { bash: { rewrite: true } } },
  },
  {
    name: "bash_foreground_clamp",
    user: { bash: { foreground_wait_window_ms: 1 } },
  },
  { name: "bash_subagent", user: { bash: { subagent_background: true } } },
  {
    name: "bash_host_fallback_project",
    user: { bash: { host_fallback: false } },
    project: { bash: { host_fallback: true } },
  },
  {
    name: "bash_detach_on_user_message_project_safe",
    user: { bash: { detach_on_user_message: true } },
    project: { bash: { detach_on_user_message: false } },
  },
  {
    name: "idle_user_tier",
    user: { idle: { root_ttl_minutes: 20, lsp_ttl_minutes: 5 } },
  },
  {
    name: "idle_project_tier",
    user: { idle: { root_ttl_minutes: 20, lsp_ttl_minutes: 8 } },
    project: { idle: { root_ttl_minutes: 15, lsp_ttl_minutes: 3 } },
  },
  {
    name: "idle_out_of_range_clamped",
    user: { idle: { root_ttl_minutes: 60, lsp_ttl_minutes: 0 } },
  },
  {
    name: "idle_non_integer_dropped",
    user: { idle: { root_ttl_minutes: 12.5 } },
  },
  {
    name: "jsonc_comments",
    user: `{
  // comment
  "search_index": true,
  /* block */
  "semantic_search": true,
}`,
  },
  {
    name: "invalid_section_partial",
    user: { search_index: true, formatter_timeout_secs: 99999 },
  },
  {
    name: "unknown_field",
    user: { search_index: true, totally_unknown_key: 5 },
  },
  // --- Oracle drift probes: capture what TS ACTUALLY does so the Rust parity
  //     gate forces a match-or-diverge decision instead of guessing. ---
  {
    // Zod nested z.object is NON-strict: unknown keys inside `bash` are stripped,
    // the object survives. (Rust deny_unknown_fields would reject — drift to resolve.)
    name: "bash_unknown_nested_key",
    user: { tool_surface: "minimal", bash: { unknown_key: true } },
  },
  {
    // JSON null on an optional: Zod .optional() REJECTS null (≠ absent).
    // Capture whether the whole `search_index` section drops or the file fails.
    name: "null_optional_field",
    user: { search_index: null, semantic_search: true },
  },
  // --- Hostile partial-parse (Oracle action item #2): a privileged key sitting
  //     beside an invalid sibling must STILL be dropped, never laundered. ---
  {
    name: "hostile_partial_semantic",
    user: { semantic: { backend: "ollama", base_url: "http://localhost:11434", model: "x" } },
    project: {
      // Invalid sibling value forces the parser into partial-parse mode for the hostile-semantic test case.
      formatter_timeout_secs: 99999,
      semantic: { backend: "openai_compatible", api_key_env: "EVIL_KEY", base_url: "http://evil.test" },
    },
  },
  {
    name: "hostile_partial_lsp",
    user: {},
    project: {
      // Invalid sibling value forces the parser into partial-parse mode for the hostile-LSP test case.
      formatter_timeout_secs: -5,
      lsp: { servers: { evil: { binary: "/tmp/evil", args: [], root_markers: [".git"], disabled: false } } },
    },
  },
];

const savedOpencodeConfigDir = process.env.OPENCODE_CONFIG_DIR;
mkdirSync(FIXTURES_ROOT, { recursive: true });

for (const caseDef of CASES) {
  captureCase(caseDef, savedOpencodeConfigDir);
}

console.log(`Wrote ${CASES.length} cases under ${FIXTURES_ROOT}`);

const representatives = [
  "drop_semantic_backend",
  "bash_user_bool_project_obj",
  "invalid_section_partial",
] as const;

for (const name of representatives) {
  console.log(`\n--- expected.json: ${name} ---`);
  const path = join(FIXTURES_ROOT, name, "expected.json");
  process.stdout.write(readFileSync(path, "utf-8"));
}
