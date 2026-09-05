/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { fixPluginEntries } from "../../commands/doctor.js";
import { getSelfVersion } from "../../lib/self-version.js";
import { OpenCodeAdapter } from "../opencode.js";

class InstalledOpenCodeAdapter extends OpenCodeAdapter {
  override isInstalled(): boolean {
    return true;
  }
}

const PINNED_PLUGIN_ENTRY = `@cortexkit/aft-opencode@${getSelfVersion()}`;

const JSONC_WITH_COMMENTS = `{
  // keep top-level comment
  "theme": "dark",
  // keep plugin comment
  "plugin": [
    // keep existing plugin comment
    "some-other-plugin"
  ]
}
`;

describe("OpenCodeAdapter JSONC preservation", () => {
  let configDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    configDir = mkdtempSync(join(tmpdir(), "aft-opencode-jsonc-"));
    mkdirSync(configDir, { recursive: true });
    releaseEnv = await acquireEnv({ OPENCODE_CONFIG_DIR: configDir });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
  });

  test("ensurePluginEntry preserves opencode.jsonc comments used by setup", async () => {
    const configPath = join(configDir, "opencode.jsonc");
    writeFileSync(configPath, JSONC_WITH_COMMENTS);

    const adapter = new OpenCodeAdapter();
    const result = await adapter.ensurePluginEntry();

    expect(result.ok).toBe(true);
    const written = readFileSync(configPath, "utf-8");
    expect(written).toContain("// keep top-level comment");
    expect(written).toContain("// keep plugin comment");
    expect(written).toContain("// keep existing plugin comment");
    expect(written).toContain(PINNED_PLUGIN_ENTRY);

    const second = await adapter.ensurePluginEntry();
    expect(second.action).toBe("already_present");
    expect(readFileSync(configPath, "utf-8")).toBe(written);
  });

  test("doctor fix path preserves opencode.jsonc comments when registering plugin", async () => {
    const configPath = join(configDir, "opencode.jsonc");
    writeFileSync(configPath, JSONC_WITH_COMMENTS);

    await fixPluginEntries([new InstalledOpenCodeAdapter()]);

    const written = readFileSync(configPath, "utf-8");
    expect(written).toContain("// keep top-level comment");
    expect(written).toContain("// keep plugin comment");
    expect(written).toContain("// keep existing plugin comment");
    expect(written).toContain(PINNED_PLUGIN_ENTRY);
  });
});

describe("OpenCodeAdapter TUI plugin entry (setup/doctor-owned registration)", () => {
  let configDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    configDir = mkdtempSync(join(tmpdir(), "aft-opencode-tui-"));
    mkdirSync(configDir, { recursive: true });
    releaseEnv = await acquireEnv({ OPENCODE_CONFIG_DIR: configDir });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
  });

  test("ensureTuiPluginEntry creates tui.json with the plugin entry", async () => {
    const adapter = new OpenCodeAdapter();
    expect(adapter.hasTuiPluginEntry()).toBe(false);

    const result = await adapter.ensureTuiPluginEntry();

    expect(result.ok).toBe(true);
    expect(result.action).toBe("added");
    const written = readFileSync(join(configDir, "tui.json"), "utf-8");
    expect(written).toContain(PINNED_PLUGIN_ENTRY);
    expect(adapter.hasTuiPluginEntry()).toBe(true);
  });

  test("ensureTuiPluginEntry preserves tui.jsonc comments and is idempotent", async () => {
    const configPath = join(configDir, "tui.jsonc");
    writeFileSync(
      configPath,
      `{
  // user's tui comment
  "plugin": [
    // other plugin
    "some-other-tui-plugin"
  ]
}
`,
    );

    const adapter = new OpenCodeAdapter();
    const first = await adapter.ensureTuiPluginEntry();
    expect(first.ok).toBe(true);
    expect(first.action).toBe("added");

    const written = readFileSync(configPath, "utf-8");
    expect(written).toContain("// user's tui comment");
    expect(written).toContain("// other plugin");
    expect(written).toContain("some-other-tui-plugin");
    expect(written).toContain(PINNED_PLUGIN_ENTRY);

    const second = await adapter.ensureTuiPluginEntry();
    expect(second.action).toBe("already_present");
  });

  test("doctor fix path registers the TUI entry when missing", async () => {
    writeFileSync(
      join(configDir, "opencode.json"),
      `{"plugin":["@cortexkit/aft-opencode@latest"]}\n`,
    );

    await fixPluginEntries([new InstalledOpenCodeAdapter()]);

    const written = readFileSync(
      readdirSync(configDir).includes("tui.jsonc")
        ? join(configDir, "tui.jsonc")
        : join(configDir, "tui.json"),
      "utf-8",
    );
    expect(written).toContain(PINNED_PLUGIN_ENTRY);
    expect(written).not.toContain("@cortexkit/aft-opencode@latest");
  });
});
