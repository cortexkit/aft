/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { getAdapter, getAllAdapters } from "../adapters/index.js";
import { OpenCodeAdapter } from "../adapters/opencode.js";
import { PiAdapter } from "../adapters/pi.js";

describe("registry", () => {
  test("getAllAdapters returns known adapters", () => {
    const all = getAllAdapters();
    const kinds = all.map((a) => a.kind).sort();
    expect(kinds).toEqual(["opencode", "pi"]);
  });

  test("getAdapter('opencode') returns OpenCodeAdapter", () => {
    const adapter = getAdapter("opencode");
    expect(adapter.kind).toBe("opencode");
    expect(adapter.displayName).toBe("OpenCode");
  });

  test("getAdapter('pi') returns PiAdapter", () => {
    const adapter = getAdapter("pi");
    expect(adapter.kind).toBe("pi");
    expect(adapter.displayName).toBe("Pi");
  });
});

describe("OpenCodeAdapter configuration", () => {
  let tmpHome: string;
  let configDir: string;

  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tmpHome = mkdtempSync(join(tmpdir(), "aft-cli-test-"));
    configDir = join(tmpHome, ".config", "opencode");
    mkdirSync(configDir, { recursive: true });
    releaseEnv = await acquireEnv({ OPENCODE_CONFIG_DIR: configDir });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
  });

  test("hasPluginEntry returns false when no config", () => {
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  test("hasPluginEntry returns false when plugin array missing", () => {
    writeFileSync(join(configDir, "opencode.jsonc"), '{\n  "theme": "dark"\n}\n');
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  test("hasPluginEntry returns true for @latest entry", () => {
    writeFileSync(
      join(configDir, "opencode.jsonc"),
      '{\n  "plugin": ["@cortexkit/aft-opencode@latest"]\n}\n',
    );
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns true for local dev path pointing at our plugin", () => {
    // Create a fake local plugin checkout with the right package name.
    const pluginDir = join(tmpHome, "work", "opencode-plugin");
    mkdirSync(pluginDir, { recursive: true });
    writeFileSync(
      join(pluginDir, "package.json"),
      JSON.stringify({ name: "@cortexkit/aft-opencode", version: "0.0.0-dev" }),
    );
    writeFileSync(
      join(configDir, "opencode.jsonc"),
      `{\n  "plugin": [${JSON.stringify(pluginDir)}]\n}\n`,
    );
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns true for file:// URL pointing at our plugin", () => {
    const pluginDir = join(tmpHome, "work", "aft-plugin");
    mkdirSync(pluginDir, { recursive: true });
    writeFileSync(
      join(pluginDir, "package.json"),
      JSON.stringify({ name: "@cortexkit/aft-opencode" }),
    );
    writeFileSync(join(configDir, "opencode.jsonc"), `{\n  "plugin": ["file://${pluginDir}"]\n}\n`);
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns true for local entry file inside our plugin package", () => {
    const pluginDir = join(tmpHome, "work", "aft-plugin");
    const distDir = join(pluginDir, "dist");
    mkdirSync(distDir, { recursive: true });
    writeFileSync(
      join(pluginDir, "package.json"),
      JSON.stringify({ name: "@cortexkit/aft-opencode" }),
    );
    const entryFile = join(distDir, "index.js");
    writeFileSync(entryFile, "export default {};\n");
    writeFileSync(
      join(configDir, "opencode.jsonc"),
      `{
  "plugin": [${JSON.stringify(entryFile)}]
}
`,
    );

    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns false for unrelated third-party plugin path containing 'opencode-plugin'", () => {
    // Regression test: a user reported that `file:///F:/hackingtool-plugin/opencode-plugin`
    // in their config caused doctor to report AFT as registered when it wasn't, because the
    // old substring matcher (`includes("/opencode-plugin")`) accepted any path containing
    // that string. Verify the new matcher rejects unrelated plugins.
    const otherPluginDir = join(tmpHome, "hackingtool-plugin", "opencode-plugin");
    mkdirSync(otherPluginDir, { recursive: true });
    writeFileSync(
      join(otherPluginDir, "package.json"),
      JSON.stringify({ name: "some-third-party-plugin" }),
    );
    writeFileSync(
      join(configDir, "opencode.jsonc"),
      `{\n  "plugin": ["file://${otherPluginDir}"]\n}\n`,
    );
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  test("hasPluginEntry returns false for path that does not exist on disk", () => {
    writeFileSync(
      join(configDir, "opencode.jsonc"),
      '{\n  "plugin": ["/nonexistent/path/to/opencode-plugin"]\n}\n',
    );
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  test("ensurePluginEntry creates config when missing", async () => {
    const adapter = new OpenCodeAdapter();
    const result = await adapter.ensurePluginEntry();
    expect(result.ok).toBe(true);
    expect(result.action).toBe("added");
    const written = readFileSync(result.configPath, "utf-8");
    expect(written).toContain("@cortexkit/aft-opencode@latest");
  });

  test("ensurePluginEntry is idempotent", async () => {
    const adapter = new OpenCodeAdapter();
    await adapter.ensurePluginEntry();
    const second = await adapter.ensurePluginEntry();
    expect(second.ok).toBe(true);
    expect(second.action).toBe("already_present");
  });

  test("ensurePluginEntry appends to existing plugin array", async () => {
    writeFileSync(join(configDir, "opencode.jsonc"), '{\n  "plugin": ["some-other-plugin"]\n}\n');
    const adapter = new OpenCodeAdapter();
    const result = await adapter.ensurePluginEntry();
    expect(result.ok).toBe(true);
    expect(result.action).toBe("added");
    const parsed = JSON.parse(readFileSync(result.configPath, "utf-8").replace(/\/\/.*$/gm, ""));
    expect(parsed.plugin).toContain("some-other-plugin");
    expect(parsed.plugin).toContain("@cortexkit/aft-opencode@latest");
  });
});

// PiAdapter reads ~/.pi/agent/settings.json — covered by faking HOME to a
// tmp dir per test. The adapter calls `homedir()` directly, which respects
// the HOME env on Unix and USERPROFILE on Windows.
describe("PiAdapter configuration", () => {
  let tmpHome: string;
  let agentDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tmpHome = mkdtempSync(join(tmpdir(), "aft-cli-pi-test-"));
    agentDir = join(tmpHome, ".pi", "agent");
    mkdirSync(agentDir, { recursive: true });
    releaseEnv = await acquireEnv({
      HOME: tmpHome,
      USERPROFILE: process.platform === "win32" ? tmpHome : process.env.USERPROFILE,
    });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
  });

  test("hasPluginEntry returns false when no settings.json", () => {
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  test("hasPluginEntry returns false when settings.json has no packages", () => {
    writeFileSync(join(agentDir, "settings.json"), '{\n  "theme": "dark"\n}\n');
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  // Pi v0.74+ writes `npm:<spec>` for npm-installed packages
  // (package-manager.js → parseSource branch).
  test("hasPluginEntry returns true for `npm:@cortexkit/aft-pi`", () => {
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: ["npm:@cortexkit/aft-pi"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns true for pinned `npm:@cortexkit/aft-pi@1.2.3`", () => {
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: ["npm:@cortexkit/aft-pi@1.2.3"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  // Local dev mode: `pi install file:/path` writes a relative path to
  // the agentDir under `packages` (see Pi's normalizePackageSourceForSettings).
  test("hasPluginEntry returns true for relative path to local plugin checkout", () => {
    const pluginDir = join(agentDir, "extensions", "aft-pi");
    mkdirSync(pluginDir, { recursive: true });
    writeFileSync(
      join(pluginDir, "package.json"),
      JSON.stringify({ name: "@cortexkit/aft-pi", version: "0.0.0-dev" }),
    );
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: ["extensions/aft-pi"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns true for absolute path to local plugin checkout", () => {
    const pluginDir = join(tmpHome, "work", "aft-pi");
    mkdirSync(pluginDir, { recursive: true });
    writeFileSync(
      join(pluginDir, "package.json"),
      JSON.stringify({ name: "@cortexkit/aft-pi", version: "0.0.0-dev" }),
    );
    writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ packages: [pluginDir] }));
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns true for `file:` URL pointing at our plugin", () => {
    const pluginDir = join(tmpHome, "work", "aft-pi-file");
    mkdirSync(pluginDir, { recursive: true });
    writeFileSync(join(pluginDir, "package.json"), JSON.stringify({ name: "@cortexkit/aft-pi" }));
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: [`file:${pluginDir}`] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry returns false for unrelated `npm:other-pkg` entry", () => {
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: ["npm:some-other-extension"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  // Regression: previously `includes("aft-pi")` would match any package whose
  // name contained "aft-pi" — e.g. a hostile package named `awesome-aft-pi-thief`.
  test("hasPluginEntry returns false for unrelated package whose name contains 'aft-pi'", () => {
    const pluginDir = join(tmpHome, "work", "aft-pi-thief");
    mkdirSync(pluginDir, { recursive: true });
    writeFileSync(
      join(pluginDir, "package.json"),
      JSON.stringify({ name: "some-unrelated-aft-pi-thief" }),
    );
    writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ packages: [pluginDir] }));
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  test("hasPluginEntry returns false for path that does not exist on disk", () => {
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: ["/nonexistent/path/to/aft-pi"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });

  // Back-compat: pre-v0.74 used extensions.json
  test("hasPluginEntry falls back to legacy extensions.json `extensions` array", () => {
    writeFileSync(
      join(agentDir, "extensions.json"),
      JSON.stringify({ extensions: ["npm:@cortexkit/aft-pi"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("hasPluginEntry parses legacy extensions.jsonc with comments", () => {
    writeFileSync(
      join(agentDir, "extensions.jsonc"),
      `{
  // Older Pi configs commonly allowed comments here.
  "extensions": ["npm:@cortexkit/aft-pi"]
}
`,
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(true);
  });

  test("settings.json `packages` takes priority over legacy extensions.json", () => {
    // settings.json has no AFT, legacy has AFT — should report false because
    // settings.json is the authoritative source on v0.74+.
    writeFileSync(
      join(agentDir, "settings.json"),
      JSON.stringify({ packages: ["npm:some-other"] }),
    );
    writeFileSync(
      join(agentDir, "extensions.json"),
      JSON.stringify({ extensions: ["npm:@cortexkit/aft-pi"] }),
    );
    const adapter = new PiAdapter();
    expect(adapter.hasPluginEntry()).toBe(false);
  });
});
