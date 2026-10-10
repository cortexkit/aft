/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  AftConfigSchema,
  resolveBridgePoolTransportOptions,
  resolveProjectOverridesForConfigure,
} from "../config.js";

const packageRoot = fileURLToPath(new URL("../../", import.meta.url));

/**
 * Fields every resolved config carries: the absent-base disabled default and
 * the default-on index switches.
 */
const RESOLVED_DEFAULTS = {
  disabled_tools: ["aft_delete", "aft_move"],
  indexes: { callgraph: true, semantic: true, trigram: true },
};
const tempRoots = new Set<string>();

function createConfigFixture() {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-config-tests-"));
  tempRoots.add(root);

  const home = join(root, "home");
  const xdgConfigHome = join(root, "xdg-config");
  const userConfigDir = join(xdgConfigHome, "cortexkit");
  const projectDirectory = join(root, "project");
  const projectConfigDir = join(projectDirectory, ".cortexkit");

  mkdirSync(userConfigDir, { recursive: true });
  mkdirSync(projectConfigDir, { recursive: true });
  mkdirSync(home, { recursive: true });

  return {
    root,
    home,
    xdgConfigHome,
    projectDirectory,
    userConfigPath: join(userConfigDir, "aft.jsonc"),
    userJsonPath: join(userConfigDir, "aft.json"),
    projectConfigPath: join(projectConfigDir, "aft.jsonc"),
    projectJsonPath: join(projectConfigDir, "aft.json"),
  };
}

function spawnConfigLoader(
  projectDirectory: string,
  env: Record<string, string>,
  script = `
    import { loadAftConfig } from "./src/config.ts";
    console.log(JSON.stringify(loadAftConfig(process.env.PROJECT_DIR!)));
  `,
) {
  return spawnSync(process.execPath, ["-e", script], {
    cwd: packageRoot,
    env: {
      ...process.env,
      OPENCODE_CONFIG_DIR: "",
      AFT_LOG_STDERR: "1",
      ...env,
      PROJECT_DIR: projectDirectory,
    },
    encoding: "utf8",
  });
}

function runConfigLoader(projectDirectory: string, env: Record<string, string>) {
  const result = spawnConfigLoader(projectDirectory, env);

  expect(result.error).toBeUndefined();
  expect(result.status).toBe(0);

  return {
    stdout: result.stdout.trim(),
    stderr: result.stderr.trim(),
  };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("loadAftConfig", () => {
  test("legacy user override cannot relocate a project config file", () => {
    const fixture = createConfigFixture();
    const directory = join(fixture.projectDirectory, ".opencode");
    mkdirSync(directory, { recursive: true });
    const path = join(directory, "aft.jsonc");
    const text = '{ "experimental_bash_compress": false, "url_fetch_allow_private": true }\n';
    writeFileSync(path, text);
    const result = spawnConfigLoader(
      fixture.projectDirectory,
      {
        HOME: fixture.home,
        XDG_CONFIG_HOME: fixture.xdgConfigHome,
        OPENCODE_CONFIG_DIR: directory,
      },
      `
      import { resolvePiBootstrapConfig } from "./src/config-error-state.ts";
      const result = await resolvePiBootstrapConfig(process.env.PROJECT_DIR!, () => {});
      if (!result.ok) throw new Error(result.message);
      console.log(JSON.stringify(result.config));
    `,
    );
    expect(result.status).toBe(0);
    expect(readFileSync(path, "utf8")).toBe(text);
    expect(existsSync(fixture.userConfigPath)).toBe(false);
    expect(existsSync(fixture.projectConfigPath)).toBe(false);
    expect(JSON.parse(result.stdout).url_fetch_allow_private).not.toBe(true);
  });

  test("legacy project config is sent to Rust as the raw project tier", () => {
    const fixture = createConfigFixture();
    const path = join(fixture.projectDirectory, ".pi", "aft.json");
    const text = '{ "experimental_bash_compress": false, "url_fetch_allow_private": true }';
    mkdirSync(join(fixture.projectDirectory, ".pi"), { recursive: true });
    writeFileSync(path, text);
    const result = spawnConfigLoader(
      fixture.projectDirectory,
      { XDG_CONFIG_HOME: fixture.xdgConfigHome },
      `
      import { buildConfigTierConfigureParams } from "./src/config.ts";
      console.log(JSON.stringify(buildConfigTierConfigureParams(process.env.PROJECT_DIR!, {}).config));
    `,
    );
    expect(result.status).toBe(0);
    expect(JSON.parse(result.stdout)).toEqual([{ tier: "project", source: path, doc: text }]);
    expect(readFileSync(path, "utf8")).toBe(text);
    expect(existsSync(fixture.projectConfigPath)).toBe(false);
  });

  test("legacy user locations relocate automatically during plugin bootstrap", () => {
    for (const source of ["opencode", "pi"] as const) {
      const fixture = createConfigFixture();
      const directory =
        source === "opencode"
          ? join(fixture.xdgConfigHome, "opencode")
          : join(fixture.home, ".pi", "agent");
      mkdirSync(directory, { recursive: true });
      const path = join(directory, "aft.jsonc");
      const text = '{ "experimental_lsp_ty": true, "bash": false }\n';
      writeFileSync(path, text);
      const result = spawnConfigLoader(
        fixture.projectDirectory,
        {
          HOME: fixture.home,
          XDG_CONFIG_HOME: fixture.xdgConfigHome,
          OPENCODE_CONFIG_DIR: "",
        },
        `
        import { resolvePiBootstrapConfig } from "./src/config-error-state.ts";
        const result = await resolvePiBootstrapConfig(process.env.PROJECT_DIR!, () => {});
        if (!result.ok) throw new Error(result.message);
        console.log(JSON.stringify(result.config));
      `,
      );
      expect(result.error).toBeUndefined();
      expect(result.status).toBe(0);
      expect(existsSync(path)).toBe(false);
      expect(readFileSync(fixture.userConfigPath, "utf8")).toBe(text);
      expect(JSON.parse(result.stdout).experimental.lsp_ty).toBe(true);
      expect(JSON.parse(result.stdout).bash).toBe(false);
    }
  });

  test("legacy project config loads in place without creating a canonical file", () => {
    for (const directory of [".opencode", ".pi"]) {
      const fixture = createConfigFixture();
      const legacyDir = join(fixture.projectDirectory, directory);
      mkdirSync(legacyDir, { recursive: true });
      const path = join(legacyDir, "aft.jsonc");
      const text = '{\n // committed legacy settings\n "experimental_bash_compress": false\n}\n';
      writeFileSync(path, text);
      const result = runConfigLoader(fixture.projectDirectory, {
        HOME: fixture.home,
        XDG_CONFIG_HOME: fixture.xdgConfigHome,
      });
      expect(JSON.parse(result.stdout).bash).toEqual({
        rewrite: false,
        compress: false,
        background: false,
      });
      expect(readFileSync(path, "utf8")).toBe(text);
      expect(existsSync(fixture.projectConfigPath)).toBe(false);
      expect(result.stderr).toContain(path);
      expect(result.stderr).toContain(fixture.projectConfigPath);
      expect(result.stderr).toContain("doctor --fix");
    }
  });

  test("plugin bootstrap relocates legacy user config and leaves project config in place", () => {
    const fixture = createConfigFixture();
    const legacyUserDir = join(fixture.root, "legacy-opencode");
    const legacyProjectDir = join(fixture.projectDirectory, ".pi");
    mkdirSync(legacyUserDir, { recursive: true });
    mkdirSync(legacyProjectDir, { recursive: true });
    const text = '{ "experimental_bash_rewrite": true }\n';
    const legacyUser = join(legacyUserDir, "aft.jsonc");
    const legacyProject = join(legacyProjectDir, "aft.jsonc");
    writeFileSync(legacyUser, text);
    writeFileSync(legacyProject, text);
    const result = spawnConfigLoader(
      fixture.projectDirectory,
      {
        OPENCODE_CONFIG_DIR: legacyUserDir,
        XDG_CONFIG_HOME: fixture.xdgConfigHome,
      },
      `
      import { resolvePiBootstrapConfig } from "./src/config-error-state.ts";
      const result = await resolvePiBootstrapConfig(process.env.PROJECT_DIR!, () => {});
      if (!result.ok) throw new Error(result.message);
      console.log(JSON.stringify(result.config));
    `,
    );
    expect(result.error).toBeUndefined();
    expect(result.status).toBe(0);
    expect(existsSync(legacyUser)).toBe(false);
    expect(readFileSync(fixture.userConfigPath, "utf8")).toBe(text);
    expect(JSON.parse(result.stdout).bash.rewrite).toBe(true);
    expect(readFileSync(legacyProject, "utf8")).toBe(text);

    expect(existsSync(fixture.projectConfigPath)).toBe(false);
  });

  test("project experimental keys keep their bytes after plugin load", () => {
    const fixture = createConfigFixture();
    const text = `{
      // Committed project settings must not be rewritten by a plugin.
      "experimental_lsp_ty": true,
      "experimental_bash_rewrite": true,
      "experimental_bash_compress": false,
      "experimental_bash_background": true,
      "harnesses": {
        "opencode": { "experimental_bash_rewrite": false },
        "pi": { "experimental_lsp_ty": false }
      }
    }\n`;
    writeFileSync(fixture.projectConfigPath, text);

    runConfigLoader(fixture.projectDirectory, { XDG_CONFIG_HOME: fixture.xdgConfigHome });
    expect(readFileSync(fixture.projectConfigPath, "utf8")).toBe(text);
  });

  test("retired keys in either file load, translated, and are never refused", () => {
    const fixture = createConfigFixture();
    const userText = JSON.stringify({
      tool_surface: "all",
      search_index: true,
      experimental_search_index: false,
      semantic_search: false,
      experimental_semantic_search: true,
      callgraph_store: false,
      idle: { lsp_ttl_minutes: 20 },
    });
    const projectText = JSON.stringify({
      search_index: false,
      inspect: { tier2_soft_deadline_ms: 50 },
      idle: { lsp_ttl_minutes: 10 },
    });
    writeFileSync(fixture.userConfigPath, userText);
    writeFileSync(fixture.projectConfigPath, projectText);
    const merged = JSON.parse(
      runConfigLoader(fixture.projectDirectory, {
        HOME: fixture.home,
        XDG_CONFIG_HOME: fixture.xdgConfigHome,
      }).stdout,
    );
    expect(merged.disabled_tools).toEqual([]);
    expect(merged.indexes).toEqual({ trigram: false, semantic: false, callgraph: false });
    expect(merged.lsp.idle_minutes).toBe(10);
    // Loading never writes either file for retired keys.
    expect(readFileSync(fixture.userConfigPath, "utf8")).toBe(userText);
    expect(readFileSync(fixture.projectConfigPath, "utf8")).toBe(projectText);
  });

  test("inspect and LSP resource settings are tighten-only", () => {
    const fixture = createConfigFixture();
    const env = { HOME: fixture.home, XDG_CONFIG_HOME: fixture.xdgConfigHome };
    mkdirSync(env.HOME, { recursive: true });
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ lsp: { idle_minutes: 30 }, inspect: { categories: { dead_code: false } } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: { idle_minutes: "never" },
        inspect: { categories: { dead_code: true, todos: false } },
      }),
    );
    const merged = JSON.parse(runConfigLoader(fixture.projectDirectory, env).stdout);
    expect(merged.lsp.idle_minutes).toBe(30);
    expect(merged.inspect.categories).toEqual({ dead_code: false, todos: false });
    writeFileSync(fixture.userConfigPath, JSON.stringify({ lsp: { idle_minutes: "never" } }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ lsp: { idle_minutes: 5 } }));
    expect(JSON.parse(runConfigLoader(fixture.projectDirectory, env).stdout).lsp.idle_minutes).toBe(
      5,
    );
  });

  test("inspect categories use exact boolean keys and idle minutes clamp the new range", () => {
    expect(AftConfigSchema.parse({ lsp: { idle_minutes: 0 } }).lsp?.idle_minutes).toBe(5);
    expect(AftConfigSchema.parse({ lsp: { idle_minutes: 2000 } }).lsp?.idle_minutes).toBe(1440);
    expect(AftConfigSchema.parse({ lsp: { idle_minutes: "never" } }).lsp?.idle_minutes).toBe(
      "never",
    );
    expect(AftConfigSchema.safeParse({ inspect: { categories: { metrics: false } } }).success).toBe(
      false,
    );
    expect(
      AftConfigSchema.safeParse({ inspect: { categories: { dead_code: "false" } } }).success,
    ).toBe(false);
  });
  test("preserves explicit empty disables and ignores project host-slot disables", () => {
    const fixture = createConfigFixture();
    const env = { HOME: fixture.home, XDG_CONFIG_HOME: fixture.xdgConfigHome };
    writeFileSync(fixture.userConfigPath, JSON.stringify({ disabled_tools: [] }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ disabled_tools: [] }));
    expect(
      JSON.parse(runConfigLoader(fixture.projectDirectory, env).stdout).disabled_tools,
    ).toEqual([]);

    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ disabled_tools: ["read", "bash", "aft_zoom"] }),
    );
    expect(
      JSON.parse(runConfigLoader(fixture.projectDirectory, env).stdout).disabled_tools,
    ).toEqual(["aft_zoom"]);
  });

  test("github honors only the user tier and warns for project overrides", () => {
    const fixture = createConfigFixture();
    const env = { HOME: fixture.home, XDG_CONFIG_HOME: fixture.xdgConfigHome };

    writeFileSync(fixture.userConfigPath, JSON.stringify({ github: { read: false } }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ github: { read: true } }));
    const disabled = runConfigLoader(fixture.projectDirectory, env);
    expect(JSON.parse(disabled.stdout)).toMatchObject({ github: { read: false } });
    expect(disabled.stderr).toContain("Ignoring github from project config");

    writeFileSync(fixture.userConfigPath, JSON.stringify({ github: { read: true } }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ github: { read: false } }));
    const enabled = runConfigLoader(fixture.projectDirectory, env);
    expect(JSON.parse(enabled.stdout)).toMatchObject({ github: { read: true } });
    expect(enabled.stderr).toContain("Ignoring github from project config");
  });

  test("retired GitHub aliases translate, and a canonical leaf beside them wins", () => {
    const fixture = createConfigFixture();
    const env = { HOME: fixture.home, XDG_CONFIG_HOME: fixture.xdgConfigHome };
    const github = () => JSON.parse(runConfigLoader(fixture.projectDirectory, env).stdout).github;

    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ github: { read: false }, gh_read: { enabled: true } }),
    );
    expect(github()).toMatchObject({ read: false });
    writeFileSync(fixture.userConfigPath, JSON.stringify({ gh_shim: { enabled: false } }));
    expect(github()).toMatchObject({ shim: false });
    // A project may not set github.read, so a project's gh_read alias, once
    // translated to github.read, is ignored and the user's value stays.
    writeFileSync(fixture.userConfigPath, JSON.stringify({ github: { read: false } }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ gh_read: { enabled: true } }));
    expect(github()).toMatchObject({ read: false });
  });

  test("edit_mode uses ordinary project-over-user precedence", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ edit_mode: "hashline" }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ edit_mode: "default" }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect((JSON.parse(result.stdout) as { edit_mode?: string }).edit_mode).toBe("default");
  });

  test("selects the Pi harness override from a shared config", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        hoist_builtin_tools: false,
        harnesses: {
          opencode: { hoist_builtin_tools: true },
          pi: { hoist_builtin_tools: false },
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    // The Pi block adds the host disables from its legacy hoist false.
    expect(JSON.parse(result.stdout).disabled_tools).toEqual([
      "aft_delete",
      "aft_move",
      "apply_patch",
      "bash",
      "edit",
      "glob",
      "grep",
      "read",
      "write",
    ]);
  });

  test("ignores nested Pi harnesses with a warning", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        harnesses: {
          pi: {
            hoist_builtin_tools: false,
            harnesses: { opencode: { hoist_builtin_tools: true } },
          },
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout).disabled_tools).toEqual(
      [
        "apply_patch",
        "bash",
        "edit",
        "glob",
        "grep",
        "read",
        "write",
        "aft_delete",
        "aft_move",
      ].sort(),
    );
    expect(result.stderr).toContain("Ignoring nested harnesses in harnesses.pi");
  });

  test("git.co_author uses ordinary project-over-user precedence", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ git: { co_author: "auto" } }));
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ git: { co_author: "AFT Pair <pair@example.test>" } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout).git).toEqual({
      co_author: "AFT Pair <pair@example.test>",
    });
    expect(result.stderr).not.toContain("Ignoring git");
  });

  test("unknown edit_mode warns, falls back to default, and preserves valid keys", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ edit_mode: "hashline" }));
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ edit_mode: "future", format_on_edit: true }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });
    const loaded = JSON.parse(result.stdout) as { edit_mode?: string; format_on_edit?: boolean };

    expect(loaded.edit_mode).toBe("default");
    expect(loaded.format_on_edit).toBe(true);
    expect(result.stderr).toContain("edit_mode");
  });

  test("legacy project enabled:false disables only unprotected tools", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ enabled: true }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ enabled: false }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as { disabled_tools: string[]; enabled?: boolean };
    expect(config.enabled).toBeUndefined();
    for (const protectedName of ["aft_safety", "read", "write", "edit", "grep", "glob", "bash"]) {
      expect(config.disabled_tools).not.toContain(protectedName);
    }
    expect(config.disabled_tools).toContain("aft_zoom");
    expect(result.stderr).toContain("disabled_tools.aft_safety");
  });

  test("legacy user enabled:false disables every tool and project enabled:true adds nothing", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ enabled: false }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ enabled: true }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect((JSON.parse(result.stdout) as { disabled_tools: string[] }).disabled_tools).toHaveLength(
      23,
    );
  });

  test("project hoist_builtin_tools:false cannot disable host slots", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ hoist_builtin_tools: true }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ hoist_builtin_tools: false }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect((JSON.parse(result.stdout) as { disabled_tools: string[] }).disabled_tools).toEqual([
      "aft_delete",
      "aft_move",
    ]);
    expect(result.stderr).toContain("disabled_tools.read");
  });

  test("honors user backup config and ignores project backup config", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ backup: { enabled: false, max_depth: 7, max_file_size: 1024 } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ backup: { enabled: true, max_depth: 1 } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.backup).toEqual({ enabled: false, max_depth: 7, max_file_size: 1024 });
    expect(result.stderr).toContain("Ignoring backup from project config");
  });

  test("ignores project-level aft_safety disable while preserving user-level aft_safety disable", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ disabled_tools: ["aft_safety"] }));
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ disabled_tools: ["aft_safety", "aft_move"] }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.disabled_tools).toContain("aft_safety");
    expect(config.disabled_tools).toContain("aft_move");
    expect(config.disabled_tools).toHaveLength(2);
  });

  test("strips project-only aft_safety from disabled_tools", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ disabled_tools: ["aft_callgraph"] }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ disabled_tools: ["aft_safety"] }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.disabled_tools).toEqual(["aft_callgraph"]);
  });

  test("project config can override callgraph store chunking knobs", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ indexes: { callgraph: true }, callgraph_chunk_size: 100 }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ indexes: { callgraph: false }, callgraph_chunk_size: 3 }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.indexes.callgraph).toBe(false);
    expect(config.callgraph_chunk_size).toBe(3);
    expect(result.stderr).not.toContain("Ignoring callgraph_store");
    expect(result.stderr).not.toContain("Ignoring callgraph_chunk_size");
  });

  test("loads a config with comments inside nested objects (issue #88)", () => {
    const fixture = createConfigFixture();
    // comment-json attaches Symbol(before:<key>) properties for the comments.
    // Before the fix, Zod stringified those symbols while building validation
    // paths and threw "Cannot convert a symbol to a string", which the outer
    // catch swallowed and silently dropped the entire config to defaults.
    writeFileSync(
      fixture.userConfigPath,
      `{
        "indexes": { "trigram": true, "semantic": true },
        "formatter": {
          // typescript uses biome
          "typescript": "biome"
        },
        "lsp": {
          "servers": {
            // my custom server
            "my-server": { "extensions": [".foo"], "binary": "my-lsp" }
          }
        }
      }`,
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const loaded = JSON.parse(result.stdout);
    expect(loaded.indexes).toEqual({ callgraph: true, semantic: true, trigram: true });
    expect(loaded.formatter).toEqual({ typescript: "biome" });
    expect(loaded.lsp?.servers?.["my-server"]?.binary).toBe("my-lsp");
    expect(result.stderr).not.toContain("Cannot convert a symbol to a string");
  });

  test("getConfigLoadErrors records parse failures and absent files do not", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.projectConfigPath, "i{ invalid");

    const script = `
      import { loadAftConfig, getConfigLoadErrors } from "./src/config.ts";
      const config = loadAftConfig(process.env.PROJECT_DIR!);
      console.log(JSON.stringify({ config, errors: getConfigLoadErrors() }));
    `;
    const bad = spawnSync(process.execPath, ["-e", script], {
      cwd: packageRoot,
      env: {
        ...process.env,
        AFT_LOG_STDERR: "1",
        HOME: fixture.home,
        XDG_CONFIG_HOME: fixture.xdgConfigHome,
        PROJECT_DIR: fixture.projectDirectory,
      },
      encoding: "utf8",
    });
    expect(bad.status).toBe(0);
    const badParsed = JSON.parse(bad.stdout.trim()) as {
      errors: Array<{ path: string }>;
    };
    expect(badParsed.errors).toHaveLength(1);
    expect(badParsed.errors[0].path).toBe(fixture.projectConfigPath);

    const empty = createConfigFixture();
    const ok = spawnSync(process.execPath, ["-e", script], {
      cwd: packageRoot,
      env: {
        ...process.env,
        AFT_LOG_STDERR: "1",
        HOME: empty.home,
        XDG_CONFIG_HOME: empty.xdgConfigHome,
        PROJECT_DIR: empty.projectDirectory,
      },
      encoding: "utf8",
    });
    expect(ok.status).toBe(0);
    expect((JSON.parse(ok.stdout.trim()) as { errors: unknown[] }).errors).toEqual([]);
  });

  test("loads user object-map lsp servers with entry defaults", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify(
        {
          lsp: {
            servers: {
              tinymist: {
                extensions: [".typ"],
                binary: "tinymist",
              },
            },
          },
        },
        null,
        2,
      ),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout)).toEqual({
      ...RESOLVED_DEFAULTS,
      lsp: {
        servers: {
          tinymist: {
            extensions: [".typ"],
            binary: "tinymist",
            args: [],
            root_markers: [".git"],
            disabled: false,
          },
        },
      },
    });
  });

  test("rejects malformed lsp servers but keeps other config sections", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify(
        {
          format_on_edit: false,
          lsp: {
            servers: {
              // `extensions` as a string (not an array) is malformed under the
              // schema. (Omitting extensions/binary entirely is now a valid
              // partial built-in override, so the malformed case must use a
              // genuinely wrong shape.)
              tinymist: {
                extensions: ".typ",
              },
            },
          },
        },
        null,
        2,
      ),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config.format_on_edit).toBe(false);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain("Partial config loaded — invalid sections skipped");
  });

  test("merges safe lsp fields while stripping project lsp servers", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        lsp: {
          servers: {
            tinymist: { extensions: [".typ"], binary: "tinymist" },
          },
          disabled: ["pyright"],
        },
      }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          servers: {
            bashls: { extensions: ["sh"], binary: "bash-language-server" },
          },
          disabled: ["yamlls"],
          python: "ty",
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(Object.keys(config.lsp.servers).sort()).toEqual(["tinymist"]);
    // Project lsp.disabled is stripped — only user-level disabled survives.
    expect(config.lsp.disabled).toEqual(["pyright"]);
    expect(config.lsp.python).toBe("ty");
    expect(result.stderr).toContain(
      `Ignoring lsp.servers, lsp.disabled from project config ${fixture.projectConfigPath}`,
    );
  });

  test("strips project lsp.servers while preserving user lsp.servers", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        lsp: {
          servers: {
            tinymist: { extensions: [".typ"], binary: "tinymist" },
          },
        },
      }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          servers: {
            evil: { extensions: [".evil"], binary: "./node_modules/.bin/evil-lsp" },
          },
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(Object.keys(config.lsp.servers)).toEqual(["tinymist"]);
    expect(config.lsp.servers.tinymist.binary).toBe("tinymist");
    expect(config.lsp.servers.evil).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.servers from project config ${fixture.projectConfigPath}`,
    );
  });

  test("strips project lsp.versions", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          versions: { "typescript-language-server": "999.0.0" },
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.versions from project config ${fixture.projectConfigPath}`,
    );
  });

  test("strips project lsp.auto_install", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          auto_install: false,
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.auto_install from project config ${fixture.projectConfigPath}`,
    );
  });

  test("strips project lsp.grace_days", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          // grace_days schema is .positive() now; use 1 to
          // exercise strip behavior with a schema-valid security-relevant value.
          grace_days: 1,
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.grace_days from project config ${fixture.projectConfigPath}`,
    );
  });

  // Project lsp.disabled is now stripped (user-only).
  test("strips project lsp.disabled", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          disabled: ["pyright", "yamlls"],
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.disabled from project config ${fixture.projectConfigPath}`,
    );
  });

  test("preserves project lsp.diagnostics_on_edit", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ lsp: { diagnostics_on_edit: false } }));
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ lsp: { diagnostics_on_edit: true } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp.diagnostics_on_edit).toBe(true);
    expect(result.stderr).not.toContain("these LSP settings only honor user-level config");
  });

  test("preserves project lsp.python", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          python: "ty",
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp.python).toBe("ty");
    expect(result.stderr).not.toContain("these LSP settings only honor user-level config");
  });

  // v0.27.2 bash graduation: nested `experimental.bash.*` legacy values are
  // migrated to top-level `bash` during load (and on the on-disk rewrite).
  // Tests below assert the post-migration shape and the new top-level
  // surface. The legacy nested input shape stays accepted for backward
  // compat (see migration tests further down).
  test("user config can set bash.rewrite via legacy experimental block", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { rewrite: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    // Graduation materializes implicit false sub-features so post-migration
    // runtime matches pre-migration runtime (where unset sub-flags were off).
    expect(config).toMatchObject({
      bash: { rewrite: true, compress: false, background: false },
    });
    expect(config).not.toHaveProperty("experimental");
    expect(result.stderr).not.toContain("Ignoring");
  });

  test("project config can override bash.rewrite via legacy experimental block", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { rewrite: true } } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ experimental: { bash: { rewrite: false } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    // Project's false value wins over user's true after graduation.
    expect(config).toMatchObject({
      bash: { rewrite: false, compress: false, background: false },
    });
    expect(result.stderr).not.toContain("Ignoring experimental from project config");
  });

  test("user config can set bash.compress via legacy experimental block", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { compress: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({
      bash: { compress: true, rewrite: false, background: false },
    });
    expect(result.stderr).not.toContain("Ignoring");
  });

  test("project config can override bash.compress via legacy experimental block", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { compress: false } } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ experimental: { bash: { compress: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({
      bash: { compress: true, rewrite: false, background: false },
    });
    expect(result.stderr).not.toContain("Ignoring experimental from project config");
  });

  test("user config can set bash.background via legacy experimental block", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { background: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({
      bash: { background: true, rewrite: false, compress: false },
    });
    expect(result.stderr).not.toContain("Ignoring");
  });

  test("project config can set bash.background via legacy experimental block", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({}));
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ experimental: { bash: { background: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({
      bash: { background: true, rewrite: false, compress: false },
    });
    expect(result.stderr).not.toContain("Ignoring experimental from project config");
  });

  test("deep merges top-level bash config across user + project", () => {
    // Post-graduation supported pattern: both files use the new top-level
    // `bash` shape, sub-features deep-merge with override winning per key.
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ bash: { rewrite: true }, experimental: { lsp_ty: true } }),
    );
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ bash: { compress: false } }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    // Field-by-field union: user's rewrite=true survives, project's
    // compress=false wins, background not set so it defaults true at
    // resolve time (resolver fills in the new graduated default).
    expect(JSON.parse(result.stdout)).toMatchObject({
      bash: { rewrite: true, compress: false },
      experimental: { lsp_ty: true },
    });
  });

  test("legacy experimental.bash in both files: project's materialized shape wins on merge", () => {
    // Cross-file legacy bash merge is a known behavior change after
    // graduation: both files materialize their experimental block into the
    // top-level shape with all three sub-features set, and the merge then
    // takes project's whole bash block wholesale. Users wanting field-level
    // deep merge should adopt the new top-level `bash` shape (see above).
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { rewrite: true } } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ experimental: { bash: { compress: false } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout)).toMatchObject({
      bash: { rewrite: false, compress: false, background: false },
    });
  });

  test("translates all experimental keys in memory without rewriting the user file", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        experimental_search_index: false,
        experimental_semantic_search: false,
        experimental_lsp_ty: true,
        experimental_bash_rewrite: true,
        experimental_bash_compress: true,
        experimental_bash_background: true,
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    // Flat keys lift to nested experimental.bash, then graduation lifts the
    // bash block to top-level. lsp_ty stays under experimental.
    expect(JSON.parse(result.stdout)).toEqual({
      ...RESOLVED_DEFAULTS,
      indexes: { callgraph: true, semantic: false, trigram: false },
      bash: { rewrite: true, compress: true, background: true },
      experimental: { lsp_ty: true },
    });
    const unchanged = JSON.parse(readFileSync(fixture.userConfigPath, "utf-8"));
    expect(unchanged.experimental_lsp_ty).toBe(true);
    expect(unchanged.experimental_bash_rewrite).toBe(true);
    expect(unchanged.experimental_bash_compress).toBe(true);
    expect(unchanged.experimental_bash_background).toBe(true);
    expect(result.stderr).toContain("applied their current equivalents in memory");
  });

  test("repeated loads translate the same user input without writing", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_bash_rewrite: true }));

    const first = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });
    const second = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(readFileSync(fixture.userConfigPath, "utf8")).toBe(
      JSON.stringify({ experimental_bash_rewrite: true }),
    );
    expect(JSON.parse(second.stdout)).toEqual(JSON.parse(first.stdout));
  });

  test("translation leaves user JSONC comments and bytes unchanged", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      '{\n  // keep me\n  /* keep this block too */\n  "experimental_bash_rewrite": true,\n}\n',
    );

    runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const migrated = readFileSync(fixture.userConfigPath, "utf-8");
    expect(migrated).toContain("// keep me");
    expect(migrated).toContain("/* keep this block too */");
    expect(migrated).toBe(
      '{\n  // keep me\n  /* keep this block too */\n  "experimental_bash_rewrite": true,\n}\n',
    );
  });

  test("user experimental keys are left for the Rust migrator", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental_lsp_ty: true, experimental_bash_compress: true }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(result.stderr).toContain("applied their current equivalents in memory");
    expect(readFileSync(fixture.userConfigPath, "utf-8")).toBe(
      JSON.stringify({ experimental_lsp_ty: true, experimental_bash_compress: true }),
    );
  });

  test("translates project and user config independently without writes", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_lsp_ty: true }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ experimental_bash_compress: true }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    // experimental_bash_compress lifts to nested experimental.bash.compress,
    // then graduates to top-level bash.compress with materialized siblings.
    expect(JSON.parse(result.stdout)).toMatchObject({
      experimental: { lsp_ty: true },
      bash: { compress: true, rewrite: false, background: false },
    });
    expect(readFileSync(fixture.userConfigPath, "utf8")).toBe(
      JSON.stringify({ experimental_lsp_ty: true }),
    );
    expect(readFileSync(fixture.projectConfigPath, "utf8")).toBe(
      JSON.stringify({ experimental_bash_compress: true }),
    );
  });

  test("legacy index precedence: immediate legacy name beats the experimental alias", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ search_index: false, experimental_search_index: true }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout).indexes.trigram).toBe(false);
    // Ordinary loading never rewrites retired keys.
    expect(readFileSync(fixture.userConfigPath, "utf-8")).toContain("experimental_search_index");
    expect(result.stderr).toContain("superseded_legacy_config");
  });

  test("read-only user experimental keys translate without a write attempt", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_lsp_ty: true }));
    chmodSync(fixture.userConfigPath, 0o444);

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout)).toEqual({
      ...RESOLVED_DEFAULTS,
      experimental: { lsp_ty: true },
    });
    expect(result.stderr).not.toContain("Config migration could not write");
    expect(readFileSync(fixture.userConfigPath, "utf-8")).toBe(
      JSON.stringify({ experimental_lsp_ty: true }),
    );
  });

  test("accepts shared disabled_tools but strips OpenCode-only auto_update", () => {
    const mixedHarness = AftConfigSchema.safeParse({
      disabled_tools: [],
      auto_update: false,
    });
    expect(mixedHarness.success).toBe(true);
    if (mixedHarness.success) expect(mixedHarness.data).toEqual({ disabled_tools: [] });
    for (const removed of [
      "tool_surface",
      "hoist_builtin_tools",
      "search_index",
      "gh_read",
      "enabled",
    ]) {
      expect(AftConfigSchema.safeParse({ [removed]: true }).success).toBe(false);
    }

    expect(AftConfigSchema.safeParse({ genuinely_unknown_key: true }).success).toBe(false);
  });

  test("warns once when stripping the OpenCode-only auto_update key", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ auto_update: false }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(JSON.parse(result.stdout)).toEqual(RESOLVED_DEFAULTS);
    expect(result.stderr).toContain("Ignoring OpenCode-only config key `auto_update`");
    expect(result.stderr.match(/OpenCode-only config key/g)).toHaveLength(1);
  });

  test("strict cutover rejects manually re-added old keys", () => {
    expect(AftConfigSchema.safeParse({ experimental_search_index: true }).success).toBe(false);
  });

  test("accepts synapse semantic backend in Pi config schema", () => {
    expect(
      AftConfigSchema.parse({
        semantic: { backend: "synapse", model: "gte-modernbert-base-f16" },
      }).semantic,
    ).toEqual({ backend: "synapse", model: "gte-modernbert-base-f16" });
  });

  test("accepts formatter_timeout_secs in Pi config schema", () => {
    expect(AftConfigSchema.parse({ formatter_timeout_secs: 7 }).formatter_timeout_secs).toBe(7);
    expect(AftConfigSchema.safeParse({ formatter_timeout_secs: 0 }).success).toBe(false);
  });

  test("accepts oxfmt formatter in Pi config schema", () => {
    expect(AftConfigSchema.parse({ formatter: { typescript: "oxfmt" } }).formatter).toEqual({
      typescript: "oxfmt",
    });
  });

  test("keeps user executable-origin lsp settings when project also sets every lsp key", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        lsp: {
          servers: {
            tinymist: { extensions: [".typ"], binary: "tinymist" },
          },
          versions: { "typescript-language-server": "4.4.0" },
          auto_install: false,
          grace_days: 14,
          disabled: ["pyright"],
          python: "pyright",
        },
      }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({
        lsp: {
          servers: {
            evil: { extensions: [".evil"], binary: "./node_modules/.bin/evil-lsp" },
          },
          versions: {
            "typescript-language-server": "999.0.0",
            "evil/package": "1.0.0",
          },
          auto_install: true,
          // schema is .positive() now; use 1 to pass schema
          // validation, then verify strict allowlist still drops it.
          grace_days: 1,
          disabled: ["yamlls"],
          python: "ty",
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(Object.keys(config.lsp.servers)).toEqual(["tinymist"]);
    expect(config.lsp.versions).toEqual({ "typescript-language-server": "4.4.0" });
    expect(config.lsp.auto_install).toBe(false);
    expect(config.lsp.grace_days).toBe(14);
    // Only user-level disabled survives — project's ["yamlls"] is stripped.
    expect(config.lsp.disabled).toEqual(["pyright"]);
    expect(config.lsp.python).toBe("ty");
    expect(result.stderr).toContain(
      `Ignoring lsp.servers, lsp.versions, lsp.auto_install, lsp.grace_days, lsp.disabled from project config ${fixture.projectConfigPath}`,
    );
  });

  test("bridge config defaults when omitted", () => {
    expect(resolveBridgePoolTransportOptions({})).toEqual({
      timeoutMs: 30_000,
      hangThreshold: 2,
    });
  });

  test("project config cannot set bridge (strict allowlist)", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ bridge: { request_timeout_ms: 45_000, hang_threshold: 3 } }, null, 2),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ bridge: { hang_threshold: 99, request_timeout_ms: 999_999 } }, null, 2),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as {
      bridge?: { request_timeout_ms?: number; hang_threshold?: number };
    };
    expect(config.bridge).toEqual({ request_timeout_ms: 45_000, hang_threshold: 3 });
    expect(result.stderr).toContain("Ignoring bridge from project config");
  });

  // Below the one-minute minimum is a config error, not a silent clamp: the
  // invalid bash block is dropped with a message naming the key, and the rest
  // of the file still loads.
  test("bash rejects worker_wait_max_ms below 60000", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify(
        { bash: { worker_wait_max_ms: 59_999, compress: false }, format_on_edit: true },
        null,
        2,
      ),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config.bash).toBeUndefined();
    expect(config.format_on_edit).toBe(true);
    expect(result.stderr).toContain("Partial config loaded");
    expect(result.stderr).toContain("bash.worker_wait_max_ms must be at least 60000");
  });

  test("project config may set worker_wait_max_ms", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ bash: { worker_wait_max_ms: 600_000 } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ bash: { worker_wait_max_ms: 120_000 } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as { bash?: { worker_wait_max_ms?: number } };
    expect(config.bash?.worker_wait_max_ms).toBe(120_000);
  });

  test("bridge rejects request_timeout_ms below 1000 and hang_threshold below 1", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify(
        { bridge: { request_timeout_ms: 500, hang_threshold: 0 }, format_on_edit: true },
        null,
        2,
      ),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config.bridge).toBeUndefined();
    expect(config.format_on_edit).toBe(true);
    expect(result.stderr).toContain("Partial config loaded");
  });
});

describe("resolveProjectOverridesForConfigure", () => {
  test("forwards the project-settable host fallback gate", () => {
    expect(
      resolveProjectOverridesForConfigure({ disabled_tools: [], bash: { host_fallback: true } }),
    ).toMatchObject({
      bash: { host_fallback: true },
    });
  });

  test("forwards effective github gates to Rust configure", () => {
    expect(
      resolveProjectOverridesForConfigure({
        disabled_tools: [],
        github: { shim: true, read: false, write: true },
      }),
    ).toMatchObject({
      github: { shim: true, read: true, write: true },
    });
  });

  test("forwards index switches and callgraph chunking to Rust configure", () => {
    expect(
      resolveProjectOverridesForConfigure({
        disabled_tools: [],
        indexes: { callgraph: false },
        callgraph_chunk_size: 3,
      }),
    ).toMatchObject({
      disabled_tools: [],
      indexes: { callgraph: false, semantic: true, trigram: true },
      callgraph_chunk_size: 3,
    });
  });
});
