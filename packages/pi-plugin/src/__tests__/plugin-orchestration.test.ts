/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test__ } from "../index.js";

const bridge = {
  send: async (command: string, params: Record<string, unknown>) => {
    if (command === "db_get_state") return { success: true, data: { value: null } };
    if (command === "db_set_state") return { success: true, data: params };
    return { success: false };
  },
};

describe("Pi Lane G plugin orchestration regressions", () => {
  test("eager configure warnings buffer and flush exactly once on first session-bound call", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-eager-warnings-"));
    const messages: string[] = [];
    const client = { ui: { notify: (message: string) => messages.push(message) } };
    const warning = {
      kind: "formatter_not_installed" as const,
      language: "ts",
      tool: "biome",
      hint: "Install biome.",
    };
    try {
      await __test__.handleConfigureWarningsForSession({
        projectRoot: "/repo-pi-eager",
        warnings: [warning],
        bridge,
        storageDir: root,
        pluginVersion: "1.0.0",
      });
      expect(messages).toHaveLength(0);

      await __test__.handleConfigureWarningsForSession({
        projectRoot: "/repo-pi-eager",
        sessionId: "session-1",
        client,
        bridge,
        warnings: [],
        storageDir: root,
        pluginVersion: "1.0.0",
      });
      await __test__.handleConfigureWarningsForSession({
        projectRoot: "/repo-pi-eager",
        sessionId: "session-1",
        client,
        bridge,
        warnings: [],
        storageDir: root,
        pluginVersion: "1.0.0",
      });

      expect(messages).toHaveLength(1);
      expect(messages[0]).toContain("Formatter is not installed");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("config parse warnings buffer and deliver on session-bound configure flush", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-config-parse-"));
    const messages: string[] = [];
    const client = { ui: { notify: (message: string) => messages.push(message) } };
    const configPath = join(root, "aft.jsonc");
    try {
      __test__.enqueueConfigParseWarnings("/repo-pi-parse", [
        { path: configPath, message: "Unexpected token i" },
      ]);
      await __test__.handleConfigureWarningsForSession({
        projectRoot: "/repo-pi-parse",
        sessionId: "session-pi-parse",
        client,
        bridge,
        warnings: [],
        storageDir: root,
        pluginVersion: "1.0.0",
      });
      expect(messages).toHaveLength(1);
      expect(messages[0]).toContain("failed to parse and was ignored");
      expect(messages[0]).toContain("npx @cortexkit/aft doctor");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("ONNX runtime is only prepared for fastembed semantic search", () => {
    expect(__test__.shouldPrepareOnnxRuntime({ indexes: { semantic: true } })).toBe(true);
    expect(
      __test__.shouldPrepareOnnxRuntime({
        indexes: { semantic: true },
        semantic: { backend: "fastembed" },
      }),
    ).toBe(true);
    expect(
      __test__.shouldPrepareOnnxRuntime({
        indexes: { semantic: true },
        semantic: { backend: "openai_compatible" },
      }),
    ).toBe(false);
    expect(
      __test__.shouldPrepareOnnxRuntime({
        indexes: { semantic: true },
        semantic: { backend: "ollama" },
      }),
    ).toBe(false);
    expect(__test__.shouldPrepareOnnxRuntime({ indexes: { semantic: false } })).toBe(false);
    // Semantic defaults on when the switch is absent.
    expect(__test__.shouldPrepareOnnxRuntime({})).toBe(true);
  });

  test("version mismatch handler downloads matching binary and hot-swaps pool", async () => {
    const ensureCalls: string[] = [];
    const replaceCalls: string[] = [];
    const handler = __test__.createVersionMismatchHandler(
      () => ({
        replaceBinary: async (path: string) => {
          replaceCalls.push(path);
          return path;
        },
      }),
      async (version?: string) => {
        ensureCalls.push(version ?? "");
        return "/cache/aft/v1.2.3/aft";
      },
    );

    await expect(handler("1.0.0", "1.2.3")).resolves.toBe("/cache/aft/v1.2.3/aft");

    expect(ensureCalls).toEqual(["v1.2.3"]);
    expect(replaceCalls).toEqual(["/cache/aft/v1.2.3/aft"]);
  });

  test("version mismatch handler shares one in-flight hot-swap per target version", async () => {
    const ensureCalls: string[] = [];
    const replaceCalls: string[] = [];
    let resolveEnsure: (path: string) => void = () => undefined;
    const pendingEnsure = new Promise<string>((resolve) => {
      resolveEnsure = resolve;
    });
    const handler = __test__.createVersionMismatchHandler(
      () => ({
        replaceBinary: async (path: string) => {
          replaceCalls.push(path);
          return path;
        },
      }),
      async (version?: string) => {
        ensureCalls.push(version ?? "");
        return pendingEnsure;
      },
    );

    const first = handler("1.0.0", "1.2.3");
    const second = handler("1.0.1", "1.2.3");
    await Promise.resolve();
    resolveEnsure("/cache/aft/v1.2.3/aft");

    await expect(Promise.all([first, second])).resolves.toEqual([
      "/cache/aft/v1.2.3/aft",
      "/cache/aft/v1.2.3/aft",
    ]);
    expect(ensureCalls).toEqual(["v1.2.3"]);
    expect(replaceCalls).toEqual(["/cache/aft/v1.2.3/aft"]);
  });
});
