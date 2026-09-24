/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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

function runConfigLoader(projectDirectory: string, env: Record<string, string>) {
  const script = `
    import { loadAftConfig } from "./src/config.ts";
    console.log(JSON.stringify(loadAftConfig(process.env.PROJECT_DIR!)));
  `;
  const result = spawnSync(process.execPath, ["-e", script], {
    cwd: packageRoot,
    env: { ...process.env, AFT_LOG_STDERR: "1", ...env, PROJECT_DIR: projectDirectory },
    encoding: "utf8",
  });

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

  test("retired GitHub aliases reject the whole load even beside canonical leaves", () => {
    const fixture = createConfigFixture();
    const env = { HOME: fixture.home, XDG_CONFIG_HOME: fixture.xdgConfigHome };
    const script = `
      import { loadAftConfig } from "./src/config.ts";
      try {
        loadAftConfig(process.env.PROJECT_DIR!);
        console.log("loaded");
      } catch (err) {
        console.log(JSON.stringify(err.errors));
      }
    `;
    const run = () =>
      spawnSync(process.execPath, ["-e", script], {
        cwd: packageRoot,
        env: { ...process.env, ...env, PROJECT_DIR: fixture.projectDirectory },
        encoding: "utf8",
      }).stdout.trim();

    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ github: { read: false }, gh_read: { enabled: true } }),
    );
    expect(JSON.parse(run())).toEqual(["removed_config_key:gh_read:use:github.read"]);
    writeFileSync(fixture.userConfigPath, JSON.stringify({ gh_shim: { enabled: false } }));
    expect(JSON.parse(run())).toEqual(["removed_config_key:gh_shim:use:github.shim"]);
    // The supported binary override alone loads.
    writeFileSync(fixture.userConfigPath, JSON.stringify({ gh_shim: { binary_path: "/opt/aft" } }));
    expect(run()).toBe("loaded");
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

  test("migrates all old config keys to the v0.18 schema", () => {
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
    // Retired index aliases translate in memory; only `aft doctor --fix`
    // rewrites them.
    expect(readFileSync(fixture.userConfigPath, "utf-8")).toContain("experimental_search_index");
    expect(result.stderr).toContain(
      `Migrated config at ${fixture.userConfigPath}: removed experimental_lsp_ty, experimental_bash_rewrite, experimental_bash_compress, experimental_bash_background`,
    );
  });

  test("migration is idempotent", () => {
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

    expect(first.stderr).toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(second.stderr).not.toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(JSON.parse(second.stdout)).toEqual(JSON.parse(first.stdout));
  });

  test("migration preserves JSONC comments", () => {
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
    // After v0.27.2 graduation, the bash block lives at top-level and
    // `experimental` is stripped when bash was the only key inside it.
    expect(migrated).toContain('"bash"');
    expect(migrated).not.toContain("experimental_bash_rewrite");
  });

  test("migrates the CortexKit jsonc config file", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental_lsp_ty: true, experimental_bash_compress: true }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    expect(result.stderr).toContain(`Migrated config at ${fixture.userConfigPath}`);
    const migrated = readFileSync(fixture.userConfigPath, "utf-8");
    expect(migrated).toContain("lsp_ty");
    expect(migrated).not.toContain("experimental_bash_compress");
  });

  test("migrates project and user config independently", () => {
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
    expect(result.stderr).toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(result.stderr).toContain(`Migrated config at ${fixture.projectConfigPath}`);
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

  test("read-only migration warning does not fail load", () => {
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
    if (result.stderr.includes("Config migration could not write")) {
      expect(readFileSync(fixture.userConfigPath, "utf-8")).toContain("experimental_lsp_ty");
    }
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
