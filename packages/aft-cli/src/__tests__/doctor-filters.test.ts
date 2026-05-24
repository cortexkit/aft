/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import {
  type ListFiltersResponse,
  printDoctorFiltersHelp,
  renderFilterList,
  renderFilterShow,
  renderTrustedProjects,
  runDoctorFilters,
} from "../commands/doctor-filters.js";
import type { AftResponse } from "../lib/aft-bridge.js";

const originalLog = console.log;

afterEach(() => {
  console.log = originalLog;
});

function captureConsole(): string[] {
  const lines: string[] = [];
  console.log = (...args: unknown[]) => lines.push(args.join(" "));
  return lines;
}

function makeAdapter(): HarnessAdapter {
  const paths: HarnessConfigPaths = {
    configDir: "/tmp/aft-test",
    harnessConfig: "/tmp/aft-test/opencode.jsonc",
    harnessConfigFormat: "jsonc",
    aftConfig: "/tmp/aft-test/aft.jsonc",
    aftConfigFormat: "jsonc",
  };
  return {
    kind: "opencode",
    displayName: "OpenCode",
    pluginPackageName: "@cortexkit/aft-opencode",
    pluginEntryWithVersion: "@cortexkit/aft-opencode@latest",
    isInstalled: () => true,
    getHostVersion: () => "test",
    detectConfigPaths: () => paths,
    hasPluginEntry: () => true,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "already_present",
      message: "ok",
      configPath: paths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({ path: "/tmp/aft-test/plugin-cache", exists: false }),
    getStorageDir: () => "/tmp/aft-test/storage",
    getLogFile: () => "/tmp/aft-test/aft.log",
    getInstallHint: () => "Install OpenCode",
    clearPluginCache: async () => ({ action: "not_found", path: "/tmp/aft-test/plugin-cache" }),
  };
}

const sampleResponse: ListFiltersResponse = {
  success: true,
  user_dir: "/Users/test/.local/share/opencode/storage/plugin/aft/filters",
  project_dir: "/repo/.aft/filters",
  project_dir_exists: true,
  trusted_projects: ["/trusted/project"],
  filters: [
    {
      name: "make",
      source: "builtin",
      source_path: null,
      matches: ["make", "gmake"],
      description: "Compact GNU make output",
      content: '[filter]\nmatches = ["make"]\n',
      trusted: null,
    },
    {
      name: "custom-build",
      source: "project",
      source_path: "/repo/.aft/filters/custom-build.toml",
      matches: ["custom-build"],
      description: "Custom build noise",
      content: '[filter]\nmatches = ["custom-build"]\n',
      trusted: false,
    },
  ],
};

describe("doctor filters rendering", () => {
  test("renders grouped list and untrusted project label", () => {
    const output = renderFilterList(sampleResponse, "/repo");
    expect(output).toContain("TOML compression filters");
    expect(output).toContain("Built-in (1):");
    expect(output).toContain("make");
    expect(output).toContain("User (");
    expect(output).toContain("  (empty)");
    expect(output).toContain("Project (./.aft/filters, 1):");
    expect(output).toContain("custom-build");
    expect(output).toContain("untrusted — run `aft doctor filters trust` to enable");
  });

  test("renders --show with source and trust state", () => {
    const output = renderFilterShow(sampleResponse, "custom-build", "/repo");
    expect(output).toContain("Filter: custom-build");
    expect(output).toContain("Source: project (./.aft/filters/custom-build.toml)");
    expect(output).toContain("Trust: untrusted");
    expect(output).toContain("[filter]");
  });

  test("renders trusted project list", () => {
    expect(renderTrustedProjects(["/a", "/b"])).toBe("/a\n/b");
    expect(renderTrustedProjects([])).toBe("(none)");
  });

  test("prints help", () => {
    const lines = captureConsole();
    printDoctorFiltersHelp();
    expect(lines.join("\n")).toContain("aft doctor filters --show <name>");
    expect(lines.join("\n")).toContain("trust --list");
  });
});

describe("runDoctorFilters", () => {
  test("trust --list uses bridge list response", async () => {
    const lines = captureConsole();
    const code = await runDoctorFilters({
      argv: ["trust", "--list"],
      findBinary: () => "/bin/aft",
      resolveAdapters: async () => [makeAdapter()],
      sendRequests: async () => [
        { id: "cfg", success: true },
        { ...sampleResponse, id: "list", success: true } as AftResponse,
      ],
    });
    expect(code).toBe(0);
    expect(lines.join("\n")).toBe("/trusted/project");
  });

  test("trust calls trust_filter_project after selecting untrusted filter", async () => {
    const lines = captureConsole();
    const seen: string[][] = [];
    const code = await runDoctorFilters({
      argv: ["trust"],
      findBinary: () => "/bin/aft",
      resolveAdapters: async () => [makeAdapter()],
      selectMany: async () => ["custom-build"],
      sendRequests: async (_binary, requests) => {
        seen.push(requests.map((request) => request.command));
        if (requests.some((request) => request.command === "trust_filter_project")) {
          return [
            { id: "cfg", success: true },
            { id: "trust", success: true, trusted: true },
          ];
        }
        return [
          { id: "cfg", success: true },
          { ...sampleResponse, id: "list", success: true } as AftResponse,
        ];
      },
    });
    expect(code).toBe(0);
    expect(seen[0]).toEqual(["configure", "list_filters"]);
    expect(seen[1]).toEqual(["configure", "trust_filter_project"]);
    expect(lines.join("\n")).toContain("Trusted 1 project(s)");
  });

  test("untrust calls untrust_filter_project for selected paths", async () => {
    const lines = captureConsole();
    const seen: string[][] = [];
    const code = await runDoctorFilters({
      argv: ["untrust"],
      findBinary: () => "/bin/aft",
      resolveAdapters: async () => [makeAdapter()],
      selectMany: async () => ["/trusted/project"],
      sendRequests: async (_binary, requests) => {
        seen.push(requests.map((request) => request.command));
        if (requests.some((request) => request.command === "untrust_filter_project")) {
          return [
            { id: "cfg", success: true },
            { id: "untrust", success: true, trusted: false },
          ];
        }
        return [
          { id: "cfg", success: true },
          { ...sampleResponse, id: "list", success: true } as AftResponse,
        ];
      },
    });
    expect(code).toBe(0);
    expect(seen[1]).toEqual(["configure", "untrust_filter_project"]);
    expect(lines.join("\n")).toContain("Untrusted 1 project(s)");
  });
});
