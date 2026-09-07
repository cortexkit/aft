/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  ensureStorageMigrated,
  migrateAftConfigFile,
  resolveCortexKitStorageRoot,
  resolveLegacyStorageRoot,
} from "../migration.js";
import { type AftFixtureBehavior, writeAftFixture } from "./test-utils/aft-executable-fixture.js";
import { acquireEnv } from "./test-utils/env-guard.js";

const packageRoot = fileURLToPath(new URL("../../", import.meta.url));

describe("storage migration bootstrap", () => {
  let tempDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-migration-test-"));
    releaseEnv = await acquireEnv({
      XDG_DATA_HOME: tempDir,
      HOME: tempDir,
      AFT_STORAGE_DIR: undefined,
      // CI exports AFT_CACHE_DIR for the e2e suites; it outranks the XDG data
      // root in the storage ladder these tests pin.
      AFT_CACHE_DIR: undefined,
    });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
  });

  function binary(behavior: AftFixtureBehavior): string {
    const path = join(tempDir, `aft-${Math.random().toString(16).slice(2)}`);
    return writeAftFixture(path, behavior);
  }

  test("ensureStorageMigrated_no_legacy_is_noop", async () => {
    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: "/missing/aft" }),
    ).resolves.toBeUndefined();
  }, 30_000);

  test("ensureStorageMigrated_with_source_marker_backfills_target_marker", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, ".migrated_to_cortexkit"), "{}", "utf8");
    const aft = binary({ exitCode: 0 });

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft }),
    ).resolves.toBeUndefined();
  }, 30_000);

  test("ensureStorageMigrated_spawns_and_succeeds", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary({ exitCode: 0 });

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft }),
    ).resolves.toBeUndefined();
  }, 30_000);

  test("ensureStorageMigrated_throws_on_nonzero_exit", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary({ stderr: "failed\n", exitCode: 5 });

    await expect(ensureStorageMigrated({ harness: "opencode", binaryPath: aft })).rejects.toThrow(
      /exit 5.*logs\/migration\/opencode-/,
    );
  }, 30_000);

  test("ensureStorageMigrated_throws_on_timeout", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary({ sleepMs: 1_000 });

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft, timeoutMs: 10 }),
    ).rejects.toThrow(/ETIMEDOUT|timed out|spawn error/i);
  }, 30_000);

  test("resolveLegacyStorageRoot_returns_pi_fixed_path", () => {
    expect(resolveLegacyStorageRoot("pi")).toBe(
      join(process.env.HOME as string, ".pi", "agent", "aft"),
    );
  });

  test("resolveLegacyStorageRoot_returns_opencode_xdg_path", () => {
    expect(resolveLegacyStorageRoot("opencode")).toBe(
      join(tempDir, "opencode", "storage", "plugin", "aft"),
    );
  });

  test("resolveCortexKitStorageRoot_uses_new_xdg_path", () => {
    expect(resolveCortexKitStorageRoot()).toBe(join(tempDir, "cortexkit", "aft"));
  });

  test("resolveCortexKitStorageRoot_prefers_absolute_environment_override", () => {
    process.env.AFT_STORAGE_DIR = join(tempDir, "local-storage");
    expect(resolveCortexKitStorageRoot()).toBe(join(tempDir, "local-storage"));
  });

  test("resolveCortexKitStorageRoot_resolves_relative_and_tilde_overrides", () => {
    process.env.AFT_STORAGE_DIR = "./relative-storage/../local-storage";
    expect(resolveCortexKitStorageRoot()).toBe(resolve("./local-storage"));

    process.env.AFT_STORAGE_DIR = "~/tilde-storage";
    expect(resolveCortexKitStorageRoot()).toBe(join(tempDir, "tilde-storage"));
  });

  test("resolveCortexKitStorageRoot_ignores_empty_override", () => {
    process.env.AFT_STORAGE_DIR = "";
    expect(resolveCortexKitStorageRoot()).toBe(join(tempDir, "cortexkit", "aft"));
  });
});

describe("config file migration", () => {
  let root: string;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "aft-config-migration-test-"));
  });

  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
  });

  test("a legacy source that resolves to the target is the live config — never deleted", () => {
    // OPENCODE_CONFIG_DIR can point at ~/.config/cortexkit, making the legacy
    // `<opencode-config>/aft.jsonc` the SAME file as the CortexKit target. That
    // file is the already-migrated live config; treating it as a legacy source
    // would unlinkSync it aside, fall the plugin back to defaults, and wipe the
    // fingerprint-keyed semantic index. It must be left fully intact.
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "cortexkit"), { recursive: true });
    const live = '{\n  "semantic_search": true,\n}\n';
    writeFileSync(target, live, "utf8");

    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      // Same path passed in as a "legacy source" (e.g. via a non-normalized ./).
      legacySources: [{ path: join(root, "cortexkit", ".", "aft.jsonc"), label: "OpenCode user" }],
    });

    // No migration happened, nothing was deleted, the live config is untouched.
    expect(result.migrated).toBe(false);
    expect(result.conflict).toBe(false);
    expect(existsSync(target)).toBe(true);
    expect(readFileSync(target, "utf8")).toBe(live);
    expect(existsSync(`${target}.MOVED_READPLEASE`)).toBe(false);
  });

  test("single legacy source moves to CortexKit target and leaves a marker", () => {
    const source = join(root, "opencode", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "opencode"), { recursive: true });
    const original = '{\n  // keep me\n  "semantic_search": true,\n}\n';
    writeFileSync(source, original, "utf8");

    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      legacySources: [{ path: source, label: "OpenCode user" }],
    });

    expect(result.migrated).toBe(true);
    expect(result.conflict).toBe(false);
    // Live config now lives at the CortexKit target, byte-identical to the original.
    expect(readFileSync(target, "utf8")).toBe(original);
    // The old location is moved aside so a later edit there can't silently no-op.
    expect(existsSync(source)).toBe(false);
    const marker = `${source}.MOVED_READPLEASE`;
    expect(existsSync(marker)).toBe(true);
    const markerContent = readFileSync(marker, "utf8");
    expect(markerContent).toContain(target); // points to the new location
    expect(markerContent).toContain('"semantic_search": true'); // preserves original settings
  });

  test("existing target wins permanently; a differing legacy source is preserved as <target>.<harness>_OLD", () => {
    const source = join(root, "pi", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "pi"), { recursive: true });
    mkdirSync(join(root, "cortexkit"), { recursive: true });
    writeFileSync(source, '{"semantic_search":true}\n', "utf8");
    writeFileSync(target, '{"semantic_search":false}\n', "utf8");

    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      legacySources: [{ path: source, label: "Pi user", harness: "pi" }],
      operatingHarness: "pi",
    });

    // First-opened wins: the existing target is never overwritten.
    expect(result.migrated).toBe(false);
    expect(result.conflict).toBe(false);
    expect(readFileSync(target, "utf8")).toBe('{"semantic_search":false}\n');
    // Pi's differing config is preserved beside the target for manual merge.
    const oldPath = `${target}.pi_OLD`;
    expect(existsSync(oldPath)).toBe(true);
    expect(readFileSync(oldPath, "utf8")).toContain('"semantic_search":true');
    // The legacy path is cleared so a later edit there can't silently no-op.
    expect(existsSync(source)).toBe(false);
    expect(existsSync(`${source}.MOVED_READPLEASE`)).toBe(true);
    expect(result.warnings.join("\n")).toContain(oldPath);
  });

  test("differing OpenCode + Pi configs: operating harness wins, other preserved as _OLD (no defaults fallback)", () => {
    const opencode = join(root, "opencode", "aft.jsonc");
    const pi = join(root, "pi", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "opencode"), { recursive: true });
    mkdirSync(join(root, "pi"), { recursive: true });
    const opencodeContent = '{"semantic_search":true}\n';
    const piContent = '{"semantic_search":false}\n';
    writeFileSync(opencode, opencodeContent, "utf8");
    writeFileSync(pi, piContent, "utf8");

    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      legacySources: [
        { path: opencode, label: "OpenCode user", harness: "opencode" },
        { path: pi, label: "Pi user", harness: "pi" },
      ],
      operatingHarness: "opencode",
    });

    // Operating harness (opencode) wins → its config becomes the shared target.
    // Critically the target IS written (no silent drop to defaults → no index wipe).
    expect(result.migrated).toBe(true);
    expect(result.conflict).toBe(false);
    expect(existsSync(target)).toBe(true);
    expect(readFileSync(target, "utf8")).toBe(opencodeContent);
    // Pi's differing config is preserved beside the target, not discarded.
    const piOld = `${target}.pi_OLD`;
    expect(existsSync(piOld)).toBe(true);
    expect(readFileSync(piOld, "utf8")).toContain('"semantic_search":false');
    // Both legacy originals are cleared.
    expect(existsSync(opencode)).toBe(false);
    expect(existsSync(pi)).toBe(false);
    expect(result.warnings.join("\n")).toContain(piOld);
  });

  test("operating harness selection: Pi wins when it is the operating harness", () => {
    const opencode = join(root, "opencode", "aft.jsonc");
    const pi = join(root, "pi", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "opencode"), { recursive: true });
    mkdirSync(join(root, "pi"), { recursive: true });
    const opencodeContent = '{"semantic_search":true}\n';
    const piContent = '{"semantic_search":false}\n';
    writeFileSync(opencode, opencodeContent, "utf8");
    writeFileSync(pi, piContent, "utf8");

    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      // opencode is first by list order; operatingHarness must override that.
      legacySources: [
        { path: opencode, label: "OpenCode user", harness: "opencode" },
        { path: pi, label: "Pi user", harness: "pi" },
      ],
      operatingHarness: "pi",
    });

    expect(result.migrated).toBe(true);
    expect(readFileSync(target, "utf8")).toBe(piContent);
    const opencodeOld = `${target}.opencode_OLD`;
    expect(existsSync(opencodeOld)).toBe(true);
    expect(readFileSync(opencodeOld, "utf8")).toContain('"semantic_search":true');
  });

  test("OpenCode and Pi legacy sources with identical semantics copy one", () => {
    const opencode = join(root, "opencode", "aft.jsonc");
    const pi = join(root, "pi", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "opencode"), { recursive: true });
    mkdirSync(join(root, "pi"), { recursive: true });
    const original = '{\n  "formatter": {"typescript": "biome"},\n  "semantic_search": true,\n}\n';
    writeFileSync(opencode, original, "utf8");
    writeFileSync(pi, '{"semantic_search":true,"formatter":{"typescript":"biome"}}\n', "utf8");

    const result = migrateAftConfigFile({
      scope: "project",
      targetPath: target,
      legacySources: [
        { path: opencode, label: "OpenCode project" },
        { path: pi, label: "Pi project" },
      ],
    });

    expect(result.migrated).toBe(true);
    expect(result.conflict).toBe(false);
    // first source (opencode) is the one copied to target
    expect(readFileSync(target, "utf8")).toBe(original);
    // Both legacy sources are moved aside (every matching trap, not just the copied one).
    expect(existsSync(opencode)).toBe(false);
    expect(existsSync(pi)).toBe(false);
    expect(existsSync(`${opencode}.MOVED_READPLEASE`)).toBe(true);
    expect(existsSync(`${pi}.MOVED_READPLEASE`)).toBe(true);
  });

  test("legacy source matching an existing target is moved aside, not left as a trap", () => {
    const source = join(root, "opencode", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "opencode"), { recursive: true });
    mkdirSync(join(root, "cortexkit"), { recursive: true });
    // Target already present (e.g. a prior migration or a fresh CortexKit file)
    // with semantically-identical content to the legacy source.
    writeFileSync(source, '{\n  "semantic_search": true,\n}\n', "utf8");
    writeFileSync(target, '{"semantic_search":true}\n', "utf8");

    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      legacySources: [{ path: source, label: "OpenCode user" }],
    });

    // No copy happened (target was already there), but the trap is cleared.
    expect(result.conflict).toBe(false);
    expect(readFileSync(target, "utf8")).toBe('{"semantic_search":true}\n');
    expect(existsSync(source)).toBe(false);
    expect(existsSync(`${source}.MOVED_READPLEASE`)).toBe(true);
  });

  test("pre-existing MOVED_READPLEASE marker is preserved with a suffixed marker", () => {
    const source = join(root, "opencode", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "opencode"), { recursive: true });
    const originalMarker = `${source}.MOVED_READPLEASE`;
    writeFileSync(source, '{"semantic_search":true}\n', "utf8");
    writeFileSync(originalMarker, "prior migration marker\n", "utf8");

    const warnings: string[] = [];
    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      legacySources: [{ path: source, label: "OpenCode user" }],
      logger: { warn: (msg) => warnings.push(msg) },
    });

    expect(result.migrated).toBe(true);
    expect(readFileSync(originalMarker, "utf8")).toBe("prior migration marker\n");
    expect(existsSync(`${originalMarker}.1`)).toBe(true);
    expect(readFileSync(`${originalMarker}.1`, "utf8")).toContain('"semantic_search":true');
    expect(warnings.join("\n")).toContain("Preserving existing legacy AFT marker");
  });

  test("pre-existing _OLD sidecar is preserved with a suffixed sidecar", () => {
    const source = join(root, "pi", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "pi"), { recursive: true });
    mkdirSync(join(root, "cortexkit"), { recursive: true });
    writeFileSync(source, '{"semantic_search":true}\n', "utf8");
    writeFileSync(target, '{"semantic_search":false}\n', "utf8");
    const oldPath = `${target}.pi_OLD`;
    writeFileSync(oldPath, "prior preserved config\n", "utf8");

    const warnings: string[] = [];
    const result = migrateAftConfigFile({
      scope: "user",
      targetPath: target,
      legacySources: [{ path: source, label: "Pi user", harness: "pi" }],
      operatingHarness: "pi",
      logger: { warn: (msg) => warnings.push(msg) },
    });

    expect(result.conflict).toBe(false);
    expect(readFileSync(oldPath, "utf8")).toBe("prior preserved config\n");
    expect(existsSync(`${oldPath}.1`)).toBe(true);
    expect(readFileSync(`${oldPath}.1`, "utf8")).toContain('"semantic_search":true');
    expect(result.warnings.join("\n")).toContain(`${oldPath}.1`);
    expect(warnings.join("\n")).toContain("Preserving existing pi config sidecar");
  });

  test("concurrent migrations are first-wins and leave valid target content", async () => {
    const source = join(root, "legacy", "aft.jsonc");
    const target = join(root, "cortexkit", "aft.jsonc");
    mkdirSync(join(root, "legacy"), { recursive: true });
    writeFileSync(source, '{"semantic_search":true,"formatter":{"typescript":"biome"}}\n', "utf8");

    const script = `
      import { migrateAftConfigFile } from "./src/migration.ts";
      const result = migrateAftConfigFile({
        scope: "user",
        targetPath: process.env.TARGET,
        legacySources: [{ path: process.env.SOURCE, label: "legacy" }],
      });
      console.log(JSON.stringify(result));
    `;
    const children = Array.from(
      { length: 8 },
      () =>
        new Promise<{ status: number | null; stdout: string; stderr: string }>((resolveChild) => {
          const child = spawn(process.execPath, ["-e", script], {
            cwd: packageRoot,
            env: { ...process.env, TARGET: target, SOURCE: source },
            stdio: ["ignore", "pipe", "pipe"],
          });
          let stdout = "";
          let stderr = "";
          child.stdout.on("data", (chunk) => {
            stdout += chunk;
          });
          child.stderr.on("data", (chunk) => {
            stderr += chunk;
          });
          child.on("close", (status) => resolveChild({ status, stdout, stderr }));
        }),
    );

    const results = await Promise.all(children);
    for (const child of results) {
      expect(child.status).toBe(0);
      expect(child.stderr).toBe("");
    }
    const parsed = results.map((child) => JSON.parse(child.stdout.trim()) as { migrated: boolean });
    expect(parsed.filter((result) => result.migrated).length).toBe(1);
    expect(JSON.parse(readFileSync(target, "utf8"))).toEqual({
      semantic_search: true,
      formatter: { typescript: "biome" },
    });
  });
});
