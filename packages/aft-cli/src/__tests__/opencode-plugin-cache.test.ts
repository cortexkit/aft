import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { discoverOpenCodePluginCache } from "../lib/opencode-plugin-cache.js";
import { ensurePinnedPluginConfig } from "../setup/opencode-config.js";

const roots: string[] = [];
const name = "@cortexkit/aft-opencode";
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});
function root() {
  const path = mkdtempSync(join(tmpdir(), "aft-opencode-cache-"));
  roots.push(path);
  return path;
}
function installed(path: string, version: string, main?: string) {
  const pkg = join(path, "node_modules/@cortexkit/aft-opencode");
  mkdirSync(pkg, { recursive: true });
  writeFileSync(
    join(pkg, "package.json"),
    JSON.stringify({ name, version, ...(main ? { main } : {}) }),
  );
}

describe("OpenCode plugin cache discovery", () => {
  test("doctor repairs the host-generation shape without changing a preserved spec or options", () => {
    const entry = `${name}@0.63.5`;
    const config = { plugins: [[entry, { enabled: true }], "other@1"], plugin: [entry] };
    ensurePinnedPluginConfig(config, "0.63.4", undefined, "v2", { doctor: true });
    expect(config).toEqual({
      plugins: [{ package: entry, options: { enabled: true } }, "other@1"],
      plugin: [entry],
    });
    const wrongKey = { plugin: [[entry, { enabled: true }], "other@1"] };
    ensurePinnedPluginConfig(wrongKey, "0.63.4", undefined, "v2", { doctor: true });
    expect(wrongKey).toEqual({
      plugin: [[entry, { enabled: true }], "other@1"],
      plugins: [{ package: entry, options: { enabled: true } }],
    });
  });
  test("V2 selects the numeric current generation for a tag, not newest semver or stale V1", () => {
    const cache = root();
    const entry = `${name}@latest`;
    installed(join(cache, "npm", entry, "9"), "0.64.0");
    installed(join(cache, "npm", entry, "10"), "0.63.5");
    installed(join(cache, "npm", entry, ".staging-999"), "0.99.0");
    installed(join(cache, "packages", entry), "0.62.0");
    installed(join(cache, "npm", `${name}@0.63.4`, "11"), "0.63.4");
    const found = discoverOpenCodePluginCache(cache, entry, "v2", "0.63.4", entry);
    expect(found.cached).toBe("0.63.5");
    expect(found.path).toBe(join(cache, "npm", entry, "10"));
    expect(found.manifestPath).toBe(join(found.path, "node_modules", name, "package.json"));
    expect(found.versionSource).toBe("installed");
    expect(found).not.toHaveProperty("latest");
  });

  test("an incomplete current generation does not fall back to a stale install", () => {
    const cache = root();
    const entry = `${name}@0.63.5`;
    installed(join(cache, "npm", entry, "9"), "0.63.5");
    installed(join(cache, "npm", entry, "10"), "0.63.5", "missing.js");
    installed(join(cache, "packages", entry), "0.63.5");
    const found = discoverOpenCodePluginCache(cache, entry, "v2", "0.63.4", entry);
    expect(found.cached).toBeUndefined();
    expect(found.exists).toBe(false);
    expect(found.version).toBe("0.63.5");
    expect(found.versionSource).toBe("configured");
  });

  test("V1 retains its own layout even when a V2 cache also exists", () => {
    const cache = root();
    const entry = `${name}@0.63.5`;
    installed(join(cache, "packages", entry), "0.63.5");
    installed(join(cache, "npm", entry, "10"), "0.64.0");
    expect(discoverOpenCodePluginCache(cache, entry, "v1", "0.63.4", entry).cached).toBe("0.63.5");
  });

  test("V2 resolves a bare configured package through the latest cache key", () => {
    const cache = root();
    installed(join(cache, "npm", `${name}@latest`, "10"), "0.63.5");
    const found = discoverOpenCodePluginCache(cache, name, "v2", "0.63.4", name);
    expect(found.cached).toBe("0.63.5");
    expect(found.configuredEntry).toBe(name);
    expect(found.path).toBe(join(cache, "npm", `${name}@latest`, "10"));
  });
});
