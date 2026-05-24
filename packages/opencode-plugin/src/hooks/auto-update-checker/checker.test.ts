import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import * as fs from "node:fs";
import { homedir } from "node:os";

mock.module("../../logger.js", () => ({
  log: mock(() => {}),
  warn: mock(() => {}),
  error: mock(() => {}),
}));

let importCounter = 0;

function freshCheckerImport() {
  return import(`./checker.ts?test=${importCounter++}`);
}

afterEach(() => {
  mock.restore();
});

describe("auto-update-checker/checker", () => {
  describe("extractChannel", () => {
    test("returns latest for null, empty, and normal semver", async () => {
      const { extractChannel } = await freshCheckerImport();

      expect(extractChannel(null)).toBe("latest");
      expect(extractChannel("")).toBe("latest");
      expect(extractChannel("1.0.0")).toBe("latest");
    });

    test("keeps dist-tags and extracts common prerelease channels", async () => {
      const { extractChannel } = await freshCheckerImport();

      expect(extractChannel("beta")).toBe("beta");
      expect(extractChannel("next")).toBe("next");
      expect(extractChannel("1.0.0-alpha.1")).toBe("alpha");
      expect(extractChannel("2.3.4-beta.5")).toBe("beta");
      expect(extractChannel("0.1.0-rc.1")).toBe("rc");
      expect(extractChannel("1.0.0-canary.0")).toBe("canary");
    });
  });

  describe("findPluginEntry", () => {
    test("detects bare and @latest entries as unpinned", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) =>
        String(p).includes("opencode.json"),
      );
      const readSpy = spyOn(fs, "readFileSync").mockReturnValue(
        JSON.stringify({ plugin: ["@cortexkit/aft-opencode"] }),
      );
      const { findPluginEntry } = await freshCheckerImport();

      expect(findPluginEntry("/test")).toEqual({
        entry: "@cortexkit/aft-opencode",
        isPinned: false,
        pinnedVersion: null,
        configPath: "/test/.opencode/opencode.json",
      });

      readSpy.mockReturnValue(JSON.stringify({ plugin: ["@cortexkit/aft-opencode@latest"] }));
      expect(findPluginEntry("/test")?.isPinned).toBe(false);

      existsSpy.mockRestore();
      readSpy.mockRestore();
    });

    test("detects pinned tuple entries and ignores other scoped packages", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) =>
        String(p).includes("opencode.json"),
      );
      const readSpy = spyOn(fs, "readFileSync").mockReturnValue(
        JSON.stringify({
          plugin: ["@cortexkit/other@1.0.0", ["@cortexkit/aft-opencode@0.17.1", {}]],
        }),
      );
      const { findPluginEntry } = await freshCheckerImport();

      const entry = findPluginEntry("/test");
      expect(entry?.entry).toBe("@cortexkit/aft-opencode@0.17.1");
      expect(entry?.isPinned).toBe(true);
      expect(entry?.pinnedVersion).toBe("0.17.1");

      existsSpy.mockRestore();
      readSpy.mockRestore();
    });
  });

  describe("getLocalDevVersion", () => {
    test("returns null when no local plugin path is configured", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockReturnValue(false);
      const { getLocalDevVersion } = await freshCheckerImport();

      expect(getLocalDevVersion("/test")).toBeNull();

      existsSpy.mockRestore();
    });

    test("returns version from a configured file:// local package", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockImplementation((p: fs.PathLike) => {
        const value = String(p);
        return value.includes("opencode.json") || value === "/dev/aft/package.json";
      });
      const statSpy = spyOn(fs, "statSync").mockImplementation(
        () => ({ isDirectory: () => true }) as fs.Stats,
      );
      const readSpy = spyOn(fs, "readFileSync").mockImplementation((p: fs.PathOrFileDescriptor) => {
        const value = String(p);
        if (value.includes("opencode.json")) {
          return JSON.stringify({ plugin: ["file:///dev/aft"] });
        }
        if (value === "/dev/aft/package.json") {
          return JSON.stringify({ name: "@cortexkit/aft-opencode", version: "1.2.3-dev" });
        }
        return "";
      });
      const { getLocalDevVersion } = await freshCheckerImport();

      expect(getLocalDevVersion("/test")).toBe("1.2.3-dev");

      existsSpy.mockRestore();
      statSpy.mockRestore();
      readSpy.mockRestore();
    });
  });

  describe("getCachedVersion and updatePinnedVersion", () => {
    test("reads cached version from OpenCode's scoped package cache layout", async () => {
      const packagePath = `${homedir()}/.cache/opencode/packages/@cortexkit/aft-opencode@latest/node_modules/@cortexkit/aft-opencode/package.json`;
      const existsSpy = spyOn(fs, "existsSync").mockImplementation(
        (p: fs.PathLike) => String(p) === packagePath,
      );
      const readSpy = spyOn(fs, "readFileSync").mockReturnValue(
        JSON.stringify({ name: "@cortexkit/aft-opencode", version: "0.17.2" }),
      );
      const { getCachedVersion } = await freshCheckerImport();

      expect(getCachedVersion("@cortexkit/aft-opencode@latest")).toBe("0.17.2");

      existsSpy.mockRestore();
      readSpy.mockRestore();
    });

    test("updates exact quoted pinned entry while preserving surrounding JSONC", async () => {
      const existsSpy = spyOn(fs, "existsSync").mockReturnValue(true);
      const readSpy = spyOn(fs, "readFileSync").mockReturnValue(
        '{\n  // plugins\n  "plugin": ["@cortexkit/aft-opencode@0.17.1"]\n}',
      );
      const writes: string[] = [];
      const writeSpy = spyOn(fs, "writeFileSync").mockImplementation(
        (_path: fs.PathOrFileDescriptor, data: string | NodeJS.ArrayBufferView) => {
          writes.push(String(data));
        },
      );
      const { updatePinnedVersion } = await freshCheckerImport();

      expect(
        updatePinnedVersion("/config/opencode.jsonc", "@cortexkit/aft-opencode@0.17.1", "0.17.2"),
      ).toBe(true);
      expect(writes[0]).toContain('"@cortexkit/aft-opencode@0.17.2"');
      expect(writes[0]).toContain("// plugins");

      existsSpy.mockRestore();
      readSpy.mockRestore();
      writeSpy.mockRestore();
    });
  });

  describe("getLatestVersion", () => {
    test("fetches channel dist-tag from npm registry package envelope", async () => {
      const fetchMock = mock(async () =>
        Response.json({ "dist-tags": { latest: "0.17.2", beta: "0.18.0-beta.1" } }),
      );
      const originalFetch = globalThis.fetch;
      globalThis.fetch = fetchMock;
      const { getLatestVersion } = await freshCheckerImport();

      expect(await getLatestVersion("beta", { registryUrl: "https://registry.example.test" })).toBe(
        "0.18.0-beta.1",
      );
      expect(fetchMock).toHaveBeenCalledWith(
        "https://registry.example.test/%40cortexkit/aft-opencode",
        expect.objectContaining({ headers: { Accept: "application/json" } }),
      );

      globalThis.fetch = originalFetch;
    });
  });
});
