/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { AftConfigSchema } from "../config.js";

const packageRoot = fileURLToPath(new URL("../../", import.meta.url));
const tempRoots = new Set<string>();

function createConfigFixture() {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-config-tests-"));
  tempRoots.add(root);

  const home = join(root, "home");
  const userConfigDir = join(home, ".pi", "agent");
  const projectDirectory = join(root, "project");
  const projectConfigDir = join(projectDirectory, ".pi");

  mkdirSync(userConfigDir, { recursive: true });
  mkdirSync(projectConfigDir, { recursive: true });

  return {
    root,
    home,
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
    });

    expect(JSON.parse(result.stdout)).toEqual({
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
              tinymist: {
                extensions: [".typ"],
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
    });

    const config = JSON.parse(result.stdout);
    expect(Object.keys(config.lsp.servers).sort()).toEqual(["tinymist"]);
    // Audit v0.17 #5: project lsp.disabled is stripped — only user-level disabled survives.
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
          // Audit-2 v0.17 #10: grace_days schema is .positive() now; use 1 to
          // exercise strip behavior with a schema-valid security-relevant value.
          grace_days: 1,
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.grace_days from project config ${fixture.projectConfigPath}`,
    );
  });

  // Audit v0.17 #5: project lsp.disabled is now stripped (user-only).
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
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp).toBeUndefined();
    expect(result.stderr).toContain(
      `Ignoring lsp.disabled from project config ${fixture.projectConfigPath}`,
    );
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
    });

    const config = JSON.parse(result.stdout);
    expect(config.lsp.python).toBe("ty");
    expect(result.stderr).not.toContain("these LSP settings only honor user-level config");
  });

  // v0.18 bash hoisting features: nested experimental flags are project-settable.
  test("user config can set experimental.bash.rewrite", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { rewrite: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({ experimental: { bash: { rewrite: true } } });
    expect(result.stderr).not.toContain("Ignoring");
  });

  test("project config can override experimental.bash.rewrite", () => {
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
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    // Project's false value wins over user's true.
    expect(config).toMatchObject({ experimental: { bash: { rewrite: false } } });
    expect(result.stderr).not.toContain("Ignoring experimental from project config");
  });

  test("user config can set experimental.bash.compress", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { compress: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({ experimental: { bash: { compress: true } } });
    expect(result.stderr).not.toContain("Ignoring");
  });

  test("project config can override experimental.bash.compress", () => {
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
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    // Project's true value wins over user's false.
    expect(config).toMatchObject({ experimental: { bash: { compress: true } } });
    expect(result.stderr).not.toContain("Ignoring experimental from project config");
  });

  test("user config can set experimental.bash.background", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { background: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    expect(config).toMatchObject({ experimental: { bash: { background: true } } });
    expect(result.stderr).not.toContain("Ignoring");
  });

  test("project config can set experimental.bash.background", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({}));
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ experimental: { bash: { background: true } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
    });

    const config = JSON.parse(result.stdout) as Record<string, unknown>;
    // Project's true value is accepted (user has no value set).
    expect(config).toMatchObject({ experimental: { bash: { background: true } } });
    expect(result.stderr).not.toContain("Ignoring experimental from project config");
  });

  test("deep merges nested experimental config", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ experimental: { bash: { rewrite: true }, lsp_ty: true } }),
    );
    writeFileSync(
      fixture.projectConfigPath,
      JSON.stringify({ experimental: { bash: { compress: false } } }),
    );

    const result = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(JSON.parse(result.stdout)).toMatchObject({
      experimental: { bash: { rewrite: true, compress: false }, lsp_ty: true },
    });
  });

  test("migrates all old config keys to the v0.18 schema", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({
        experimental_search_index: true,
        experimental_semantic_search: true,
        experimental_lsp_ty: true,
        experimental_bash_rewrite: true,
        experimental_bash_compress: true,
        experimental_bash_background: true,
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(JSON.parse(result.stdout)).toEqual({
      search_index: true,
      semantic_search: true,
      experimental: { bash: { rewrite: true, compress: true, background: true }, lsp_ty: true },
    });
    expect(readFileSync(fixture.userConfigPath, "utf-8")).not.toContain(
      "experimental_search_index",
    );
    expect(result.stderr).toContain(
      `Migrated config at ${fixture.userConfigPath}: removed experimental_search_index, experimental_semantic_search, experimental_lsp_ty, experimental_bash_rewrite, experimental_bash_compress, experimental_bash_background`,
    );
  });

  test("migration is idempotent", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_search_index: true }));

    const first = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });
    const second = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(first.stderr).toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(second.stderr).not.toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(JSON.parse(second.stdout)).toEqual({ search_index: true });
  });

  test("migration preserves JSONC comments", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      '{\n  // keep me\n  /* keep this block too */\n  "experimental_bash_rewrite": true,\n}\n',
    );

    runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    const migrated = readFileSync(fixture.userConfigPath, "utf-8");
    expect(migrated).toContain("// keep me");
    expect(migrated).toContain("/* keep this block too */");
    expect(migrated).toContain('"experimental"');
    expect(migrated).not.toContain("experimental_bash_rewrite");
  });

  test("migrates both jsonc and json candidate files", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_search_index: true }));
    writeFileSync(fixture.userJsonPath, JSON.stringify({ experimental_semantic_search: true }));

    const result = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(result.stderr).toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(result.stderr).toContain(`Migrated config at ${fixture.userJsonPath}`);
    expect(readFileSync(fixture.userConfigPath, "utf-8")).toContain("search_index");
    expect(readFileSync(fixture.userJsonPath, "utf-8")).toContain("semantic_search");
  });

  test("migrates project and user config independently", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_search_index: true }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ experimental_bash_compress: true }));

    const result = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(JSON.parse(result.stdout)).toMatchObject({
      search_index: true,
      experimental: { bash: { compress: true } },
    });
    expect(result.stderr).toContain(`Migrated config at ${fixture.userConfigPath}`);
    expect(result.stderr).toContain(`Migrated config at ${fixture.projectConfigPath}`);
  });

  test("migration conflict keeps new value and removes old key", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ search_index: false, experimental_search_index: true }),
    );

    const result = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(JSON.parse(result.stdout)).toEqual({ search_index: false });
    expect(readFileSync(fixture.userConfigPath, "utf-8")).not.toContain(
      "experimental_search_index",
    );
    expect(result.stderr).toContain("Config migration conflict");
  });

  test("read-only migration warning does not fail load", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ experimental_search_index: true }));
    chmodSync(fixture.userConfigPath, 0o444);

    const result = runConfigLoader(fixture.projectDirectory, { HOME: fixture.home });

    expect(JSON.parse(result.stdout)).toEqual({ search_index: true });
    if (result.stderr.includes("Config migration could not write")) {
      expect(readFileSync(fixture.userConfigPath, "utf-8")).toContain("experimental_search_index");
    }
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
          // Audit-2 v0.17 #10: schema is .positive() now; use 1 to pass schema
          // validation, then verify strict allowlist still drops it.
          grace_days: 1,
          disabled: ["yamlls"],
          python: "ty",
        },
      }),
    );

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: fixture.home,
    });

    const config = JSON.parse(result.stdout);
    expect(Object.keys(config.lsp.servers)).toEqual(["tinymist"]);
    expect(config.lsp.versions).toEqual({ "typescript-language-server": "4.4.0" });
    expect(config.lsp.auto_install).toBe(false);
    expect(config.lsp.grace_days).toBe(14);
    // Audit v0.17 #5: only user-level disabled survives — project's ["yamlls"] is stripped.
    expect(config.lsp.disabled).toEqual(["pyright"]);
    expect(config.lsp.python).toBe("ty");
    expect(result.stderr).toContain(
      `Ignoring lsp.servers, lsp.versions, lsp.auto_install, lsp.grace_days, lsp.disabled from project config ${fixture.projectConfigPath}`,
    );
  });
});
