/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { formatDroppedKeyWarnings } from "@cortexkit/aft-bridge";

import { __test__ } from "../index.js";

const GROUP_A_PROCESS_KEYS = [
  "storage_dir",
  "harness",
  "bash_permissions",
  "_ort_dylib_dir",
  "lsp_paths_extra",
  "lsp_auto_install_binaries",
  "lsp_inflight_installs",
  "max_background_bash_tasks",
  "aft_search_registered",
  "project_root",
  "cortexkit_user_config_path",
] as const;

const GROUP_B_CORE_KEYS = [
  "format_on_edit",
  "formatter_timeout_secs",
  "validate_on_edit",
  "formatter",
  "checker",
  "restrict_to_project_root",
  "search_index",
  "semantic_search",
  "callgraph_store",
  "callgraph_chunk_size",
  "experimental_bash_rewrite",
  "experimental_bash_compress",
  "experimental_bash_background",
  "bash_long_running_reminder_enabled",
  "bash_long_running_reminder_interval_ms",
  "experimental_lsp_ty",
  "lsp_servers",
  "disabled_lsp",
  "semantic",
  "inspect",
  "url_fetch_allow_private",
] as const;

const tempRoots = new Set<string>();
const originalHome = process.env.HOME;
const originalXdgConfigHome = process.env.XDG_CONFIG_HOME;
const packageRoot = fileURLToPath(new URL("../../", import.meta.url));

function createFixture() {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-config-tiers-"));
  tempRoots.add(root);
  const home = join(root, "home");
  const xdgConfigHome = join(root, "xdg-config");
  const userConfigDir = join(xdgConfigHome, "cortexkit");
  const projectDirectory = join(root, "project");
  const projectConfigDir = join(projectDirectory, ".cortexkit");
  mkdirSync(userConfigDir, { recursive: true });
  mkdirSync(projectConfigDir, { recursive: true });
  return {
    root,
    home,
    xdgConfigHome,
    projectDirectory,
    userConfigPath: join(userConfigDir, "aft.jsonc"),
    projectConfigPath: join(projectConfigDir, "aft.jsonc"),
  };
}

function representativeProcessState(root: string): Record<string, unknown> {
  return {
    storage_dir: join(root, "storage"),
    harness: "pi",
    bash_permissions: true,
    _ort_dylib_dir: join(root, "onnxruntime", "lib"),
    lsp_paths_extra: [join(root, "lsp-bin")],
    lsp_auto_install_binaries: ["typescript-language-server", "pyright"],
    lsp_inflight_installs: ["typescript-language-server"],
    max_background_bash_tasks: 4,
    aft_search_registered: true,
  };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
  restoreEnv("HOME", originalHome);
  restoreEnv("XDG_CONFIG_HOME", originalXdgConfigHome);
});

/**
 * `process.env.KEY = undefined` stores the string "undefined" (Node and Bun both
 * stringify), which then reaches every process spawned by later test files in
 * the run. A variable that was unset before the test must be deleted, not
 * assigned back.
 */
function restoreEnv(key: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[key];
  } else {
    process.env[key] = value;
  }
}

describe("Pi configure config tiers cutover", () => {
  test("sends raw config tiers plus process-state keys and no resolved core-domain flat params", () => {
    const fixture = createFixture();
    process.env.HOME = fixture.home;
    process.env.XDG_CONFIG_HOME = fixture.xdgConfigHome;

    const userDoc = JSON.stringify(
      {
        format_on_edit: false,
        formatter_timeout_secs: 12,
        url_fetch_allow_private: true,
        semantic: { backend: "ollama", model: "nomic-embed-text" },
      },
      null,
      2,
    );
    const projectDoc = JSON.stringify(
      {
        validate_on_edit: "syntax",
        formatter: { typescript: "biome" },
        checker: { typescript: "tsc" },
        restrict_to_project_root: true,
        search_index: true,
        semantic_search: true,
        callgraph_store: false,
        callgraph_chunk_size: 3,
        experimental: {
          lsp_ty: true,
        },
        bash: {
          rewrite: true,
          compress: true,
          background: false,
          long_running_reminder_enabled: true,
          long_running_reminder_interval_ms: 30_000,
        },
        lsp: { disabled: ["tsserver"], servers: { custom: { binary: "custom-lsp" } } },
        inspect: { enabled: true },
      },
      null,
      2,
    );
    writeFileSync(fixture.userConfigPath, userDoc, "utf8");
    writeFileSync(fixture.projectConfigPath, projectDoc, "utf8");

    const script = `
      import { buildConfigTierConfigureParams } from "./src/config.ts";
      const projectDirectory = process.env.PROJECT_DIR!;
      const processState = JSON.parse(process.env.PROCESS_STATE!);
      console.log(JSON.stringify({
        project_root: projectDirectory,
        ...buildConfigTierConfigureParams(projectDirectory, processState),
      }));
    `;
    const result = spawnSync(process.execPath, ["-e", script], {
      cwd: packageRoot,
      env: {
        ...process.env,
        HOME: fixture.home,
        XDG_CONFIG_HOME: fixture.xdgConfigHome,
        PROJECT_DIR: fixture.projectDirectory,
        PROCESS_STATE: JSON.stringify(representativeProcessState(fixture.root)),
      },
      encoding: "utf8",
    });
    expect(result.error).toBeUndefined();
    expect(result.status).toBe(0);
    const configureParams = JSON.parse(result.stdout.trim()) as Record<string, unknown>;

    const tiers = configureParams.config as Array<{ tier: string; source: string; doc: string }>;
    expect(Array.isArray(tiers)).toBe(true);
    expect(tiers).toEqual([
      { tier: "user", source: resolve(fixture.userConfigPath), doc: userDoc },
      { tier: "project", source: resolve(fixture.projectConfigPath), doc: projectDoc },
    ]);

    for (const key of GROUP_A_PROCESS_KEYS) {
      expect(key in configureParams).toBe(true);
    }
    for (const key of GROUP_B_CORE_KEYS) {
      expect(key in configureParams).toBe(false);
    }
  });

  test("Rust config_dropped_keys are formatted and delivered with configure warnings", async () => {
    const storageRoot = mkdtempSync(join(tmpdir(), "aft-pi-dropped-keys-"));
    tempRoots.add(storageRoot);
    const messages: string[] = [];
    const client = { ui: { notify: (message: string) => messages.push(message) } };
    const bridge = {
      send: async (command: string, params: Record<string, unknown>) => {
        if (command === "db_get_state") return { success: true, data: { value: null } };
        if (command === "db_set_state") return { success: true, data: params };
        return { success: false };
      },
    };
    const dropped = [
      {
        key: "semantic.backend",
        tier: "project",
        reason: "security: use user config for external backends",
      },
    ];

    await __test__.handleConfigureWarningsForSession({
      projectRoot: "/repo-pi-dropped",
      sessionId: "session-dropped",
      client,
      bridge,
      warnings: [],
      configDroppedKeys: dropped,
      storageDir: storageRoot,
      pluginVersion: "1.0.0",
    });

    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain(formatDroppedKeyWarnings(dropped)[0]);
  });
});
