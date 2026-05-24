import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import * as childProcess from "node:child_process";
import { EventEmitter } from "node:events";
import * as fs from "node:fs";

mock.module("../../logger.js", () => ({
  log: mock(() => {}),
  warn: mock(() => {}),
  error: mock(() => {}),
}));

/**
 * Lane G's auto-update-snapshot path (createAutoUpdateSnapshot in
 * cache.ts) calls `mkdtempSync` + `cpSync` to stage the installed
 * package directory before mutating it. Tests don't have a real
 * filesystem for those paths, so we stub both to no-op. Without these
 * stubs, `cpSync` throws ENOENT inside preparePackageUpdate's try
 * block and the function returns null (looking like a regression).
 */
function stubSnapshotFs() {
  const mkdtempSpy = spyOn(fs, "mkdtempSync").mockReturnValue("/tmp/aft-test-snapshot");
  const cpSpy = spyOn(fs, "cpSync").mockReturnValue(undefined);
  return () => {
    mkdtempSpy.mockRestore();
    cpSpy.mockRestore();
  };
}

let importCounter = 0;

function freshCacheImport() {
  return import(`./cache.ts?test=${importCounter++}`);
}

afterEach(() => {
  mock.restore();
});

describe("auto-update-checker/cache", () => {
  describe("resolveInstallContext", () => {
    test("detects OpenCode packages install root from runtime package path", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockImplementation(
        (p: fs.PathLike) =>
          String(p) ===
          "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest/package.json",
      );
      const { resolveInstallContext } = await freshCacheImport();

      expect(
        resolveInstallContext(
          "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest/node_modules/@cortexkit/aft-opencode/package.json",
        ),
      ).toEqual({
        installDir: "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest",
        packageJsonPath:
          "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest/package.json",
      });

      existsSpy.mockRestore();
    });

    test("does not fall back when runtime path exists but wrapper root is invalid", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockReturnValue(false);
      const { resolveInstallContext } = await freshCacheImport();

      expect(
        resolveInstallContext(
          "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest/node_modules/@cortexkit/aft-opencode/package.json",
        ),
      ).toBeNull();

      existsSpy.mockRestore();
    });
  });

  describe("preparePackageUpdate", () => {
    test("returns null when no install context is available", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockReturnValue(false);
      const { preparePackageUpdate } = await freshCacheImport();

      expect(preparePackageUpdate("0.17.2", "@cortexkit/aft-opencode", null)).toBeNull();

      existsSpy.mockRestore();
    });

    test("updates wrapper dependency and removes installed scoped package", async () => {
      const root = "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest";
      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) => {
        const value = String(p);
        return (
          value === `${root}/package.json` ||
          value === `${root}/node_modules/@cortexkit/aft-opencode`
        );
      });
      const readSpy = spyOn(fs, "readFileSync").mockImplementation((p: fs.PathOrFileDescriptor) => {
        if (String(p) === `${root}/package.json`) {
          return JSON.stringify({ dependencies: { "@cortexkit/aft-opencode": "0.17.1" } });
        }
        return "";
      });
      const writes: string[] = [];
      const writeSpy = spyOn(fs, "writeFileSync").mockImplementation(
        (_path: fs.PathOrFileDescriptor, data: string | NodeJS.ArrayBufferView) => {
          writes.push(String(data));
        },
      );
      const rmSpy = spyOn(fs, "rmSync").mockReturnValue(undefined);
      const restoreSnapshotFs = stubSnapshotFs();
      const { preparePackageUpdate } = await freshCacheImport();

      expect(
        preparePackageUpdate(
          "0.17.2",
          "@cortexkit/aft-opencode",
          `${root}/node_modules/@cortexkit/aft-opencode/package.json`,
        ),
      ).toBe(root);
      expect(JSON.parse(writes[0])).toEqual({
        dependencies: { "@cortexkit/aft-opencode": "0.17.2" },
      });
      expect(rmSpy).toHaveBeenCalledWith(`${root}/node_modules/@cortexkit/aft-opencode`, {
        recursive: true,
        force: true,
      });

      existsSpy.mockRestore();
      readSpy.mockRestore();
      writeSpy.mockRestore();
      rmSpy.mockRestore();
      restoreSnapshotFs();
    });

    test("does not rewrite package.json when dependency is already target version", async () => {
      const root = "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest";
      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) => {
        const value = String(p);
        return (
          value === `${root}/package.json` ||
          value === `${root}/node_modules/@cortexkit/aft-opencode`
        );
      });
      const readSpy = spyOn(fs, "readFileSync").mockReturnValue(
        JSON.stringify({ dependencies: { "@cortexkit/aft-opencode": "0.17.2" } }),
      );
      const writeSpy = spyOn(fs, "writeFileSync").mockImplementation(() => {});
      const rmSpy = spyOn(fs, "rmSync").mockReturnValue(undefined);
      const restoreSnapshotFs = stubSnapshotFs();
      const { preparePackageUpdate } = await freshCacheImport();

      expect(
        preparePackageUpdate(
          "0.17.2",
          "@cortexkit/aft-opencode",
          `${root}/node_modules/@cortexkit/aft-opencode/package.json`,
        ),
      ).toBe(root);
      expect(writeSpy).not.toHaveBeenCalled();
      expect(rmSpy).toHaveBeenCalled();

      existsSpy.mockRestore();
      readSpy.mockRestore();
      writeSpy.mockRestore();
      rmSpy.mockRestore();
      restoreSnapshotFs();
    });
  });

  describe("runNpmInstallSafe", () => {
    test("returns true for successful npm install", async () => {
      const proc = new EventEmitter();
      const spawnMock = spyOn(childProcess, "spawn").mockImplementation(() => {
        setTimeout(() => proc.emit("exit", 0), 0);
        return proc as childProcess.ChildProcess;
      });
      const { runNpmInstallSafe } = await freshCacheImport();

      expect(await runNpmInstallSafe("/tmp/opencode", { timeoutMs: 1000 })).toBe(true);
      // Critical contract: we spawn `npm install` with the quiet flags so
      // background auto-updates don't dump audit/funding output into the
      // plugin log. Earlier versions called `bun install`, which generated
      // a parallel bun.lock that drifted from OpenCode's package-lock.json.
      expect(spawnMock).toHaveBeenCalledWith(
        "npm",
        ["install", "--no-audit", "--no-fund", "--no-progress"],
        {
          cwd: "/tmp/opencode",
          // Lane G F6: stdio:"ignore" so spawned npm doesn't keep file
          // descriptors open and prevent the plugin host from exiting.
          stdio: "ignore",
        },
      );

      spawnMock.mockRestore();
    });

    test("kills install process and returns false on timeout", async () => {
      const proc = new EventEmitter() as childProcess.ChildProcess;
      const killMock = mock(() => true);
      proc.kill = killMock;
      const spawnMock = spyOn(childProcess, "spawn").mockReturnValue(proc);
      const { runNpmInstallSafe } = await freshCacheImport();

      expect(await runNpmInstallSafe("/tmp/opencode", { timeoutMs: 1 })).toBe(false);
      expect(killMock).toHaveBeenCalled();

      spawnMock.mockRestore();
    });
  });

  describe("removeFromPackageLock (via preparePackageUpdate)", () => {
    /**
     * Regression test for the magic-context-style lockfile migration: the
     * auto-update flow used to clean entries from `bun.lock`, but OpenCode
     * generates `package-lock.json` (npm v7+) so cleaning the wrong
     * lockfile silently no-ops and `npm install` reuses the stale resolved
     * version. We must clean `package-lock.json` `packages` entries keyed
     * by `node_modules/<name>` (npm v7+ shape).
     */
    test("cleans package-lock.json packages entry (npm v7+ shape)", async () => {
      const root = "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest";
      const lockPath = `${root}/package-lock.json`;
      const lockContents = JSON.stringify({
        name: "@cortexkit/aft-opencode@latest",
        lockfileVersion: 3,
        packages: {
          "": { dependencies: { "@cortexkit/aft-opencode": "0.17.1" } },
          "node_modules/@cortexkit/aft-opencode": {
            version: "0.17.1",
            resolved: "https://registry.npmjs.org/...",
          },
          "node_modules/some-other-dep": { version: "1.0.0" },
        },
      });

      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) => {
        const value = String(p);
        return (
          value === `${root}/package.json` ||
          value === lockPath ||
          value === `${root}/node_modules/@cortexkit/aft-opencode`
        );
      });
      const readSpy = spyOn(fs, "readFileSync").mockImplementation((p: fs.PathOrFileDescriptor) => {
        const value = String(p);
        if (value === `${root}/package.json`) {
          return JSON.stringify({ dependencies: { "@cortexkit/aft-opencode": "0.17.1" } });
        }
        if (value === lockPath) return lockContents;
        return "";
      });
      const writes: { path: string; data: string }[] = [];
      const writeSpy = spyOn(fs, "writeFileSync").mockImplementation(
        (path: fs.PathOrFileDescriptor, data: string | NodeJS.ArrayBufferView) => {
          writes.push({ path: String(path), data: String(data) });
        },
      );
      const rmSpy = spyOn(fs, "rmSync").mockReturnValue(undefined);
      const restoreSnapshotFs = stubSnapshotFs();

      const { preparePackageUpdate } = await freshCacheImport();
      preparePackageUpdate(
        "0.17.2",
        "@cortexkit/aft-opencode",
        `${root}/node_modules/@cortexkit/aft-opencode/package.json`,
      );

      const lockWrite = writes.find((w) => w.path === lockPath);
      expect(lockWrite).toBeDefined();
      const updatedLock = JSON.parse(lockWrite?.data ?? "{}");
      // Our package's `node_modules/...` entry should be gone — `npm install`
      // will recompute it against the new version we just wrote into
      // package.json.
      expect(updatedLock.packages["node_modules/@cortexkit/aft-opencode"]).toBeUndefined();
      // Sibling packages must NOT be touched. This guard catches the
      // accidental "delete everything" regression.
      expect(updatedLock.packages["node_modules/some-other-dep"]).toBeDefined();

      existsSpy.mockRestore();
      readSpy.mockRestore();
      writeSpy.mockRestore();
      rmSpy.mockRestore();
      restoreSnapshotFs();
    });

    test("cleans legacy npm v6 dependencies map alongside packages map", async () => {
      const root = "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest";
      const lockPath = `${root}/package-lock.json`;
      const lockContents = JSON.stringify({
        // npm v6 shape has `dependencies` only, no `packages`.
        dependencies: {
          "@cortexkit/aft-opencode": { version: "0.17.1", resolved: "..." },
          "some-other-dep": { version: "1.0.0" },
        },
      });

      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) => {
        const value = String(p);
        return (
          value === `${root}/package.json` ||
          value === lockPath ||
          value === `${root}/node_modules/@cortexkit/aft-opencode`
        );
      });
      const readSpy = spyOn(fs, "readFileSync").mockImplementation((p: fs.PathOrFileDescriptor) => {
        const value = String(p);
        if (value === `${root}/package.json`) {
          return JSON.stringify({ dependencies: { "@cortexkit/aft-opencode": "0.17.1" } });
        }
        if (value === lockPath) return lockContents;
        return "";
      });
      const writes: { path: string; data: string }[] = [];
      const writeSpy = spyOn(fs, "writeFileSync").mockImplementation(
        (path: fs.PathOrFileDescriptor, data: string | NodeJS.ArrayBufferView) => {
          writes.push({ path: String(path), data: String(data) });
        },
      );
      const rmSpy = spyOn(fs, "rmSync").mockReturnValue(undefined);
      const restoreSnapshotFs = stubSnapshotFs();

      const { preparePackageUpdate } = await freshCacheImport();
      preparePackageUpdate(
        "0.17.2",
        "@cortexkit/aft-opencode",
        `${root}/node_modules/@cortexkit/aft-opencode/package.json`,
      );

      const lockWrite = writes.find((w) => w.path === lockPath);
      expect(lockWrite).toBeDefined();
      const updatedLock = JSON.parse(lockWrite?.data ?? "{}");
      expect(updatedLock.dependencies["@cortexkit/aft-opencode"]).toBeUndefined();
      expect(updatedLock.dependencies["some-other-dep"]).toBeDefined();

      existsSpy.mockRestore();
      readSpy.mockRestore();
      writeSpy.mockRestore();
      rmSpy.mockRestore();
      restoreSnapshotFs();
    });

    test("does not touch a stale bun.lock file (no longer the install target)", async () => {
      // We deliberately NO LONGER read or write bun.lock. If a user has one
      // lingering from an older AFT install, it must be left alone — npm
      // doesn't read it and writing JSON to it could corrupt a future
      // bun-based workflow.
      const root = "/home/user/.cache/opencode/packages/@cortexkit/aft-opencode@latest";
      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) => {
        const value = String(p);
        return (
          value === `${root}/package.json` ||
          value === `${root}/bun.lock` ||
          value === `${root}/node_modules/@cortexkit/aft-opencode`
        );
      });
      const readSpy = spyOn(fs, "readFileSync").mockImplementation((p: fs.PathOrFileDescriptor) => {
        if (String(p) === `${root}/package.json`) {
          return JSON.stringify({ dependencies: { "@cortexkit/aft-opencode": "0.17.1" } });
        }
        return "";
      });
      const writes: { path: string; data: string }[] = [];
      const writeSpy = spyOn(fs, "writeFileSync").mockImplementation(
        (path: fs.PathOrFileDescriptor, data: string | NodeJS.ArrayBufferView) => {
          writes.push({ path: String(path), data: String(data) });
        },
      );
      const rmSpy = spyOn(fs, "rmSync").mockReturnValue(undefined);

      const { preparePackageUpdate } = await freshCacheImport();
      preparePackageUpdate(
        "0.17.2",
        "@cortexkit/aft-opencode",
        `${root}/node_modules/@cortexkit/aft-opencode/package.json`,
      );

      // We touched package.json but NOT bun.lock.
      expect(writes.find((w) => w.path === `${root}/bun.lock`)).toBeUndefined();

      existsSpy.mockRestore();
      readSpy.mockRestore();
      writeSpy.mockRestore();
      rmSpy.mockRestore();
    });
  });
});
