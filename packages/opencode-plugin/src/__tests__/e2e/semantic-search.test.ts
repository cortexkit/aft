/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { semanticTools } from "../../tools/semantic.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
  };
}

function createPluginContext(pool: BridgePool, storageDir: string): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir,
  };
}

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: "semantic-search-e2e",
    messageID: "semantic-search-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

maybeDescribe("e2e semantic search tool", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];
  const pools: BridgePool[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(pools.splice(0, pools.length).map((pool) => pool.shutdown()));
    await cleanupHarnesses(harnesses);
  });

  async function createToolHarness(options?: { experimentalSemanticSearch?: boolean }) {
    const harness = await createHarness(preparedBinary, {
      fixtureNames: [],
      timeoutMs: 20_000,
      tempPrefix: "aft-plugin-semantic-search-",
    });
    harnesses.push(harness);

    await createFixtureProject(harness.tempDir);

    const pool = new BridgePool(
      harness.binaryPath,
      { timeoutMs: 20_000 },
      {
        semantic_search: options?.experimentalSemanticSearch ?? false,
        storage_dir: join(harness.tempDir, ".storage"),
        harness: "opencode",
      },
    );
    pools.push(pool);

    return {
      harness,
      pool,
      sdkCtx: createSdkContext(harness.tempDir),
      tools: semanticTools(createPluginContext(pool, join(harness.tempDir, ".storage"))),
    };
  }

  test("aft_search returns not_ready text when the semantic index is unavailable", async () => {
    const { tools, sdkCtx } = await createToolHarness({ experimentalSemanticSearch: false });

    const output = await tools.aft_search.execute(
      { query: "request authentication handler" },
      sdkCtx,
    );

    expect(output).toBe("Semantic search is not enabled.");
  });

  test("aft_search handles a missing query parameter gracefully", async () => {
    const { tools, sdkCtx } = await createToolHarness({ experimentalSemanticSearch: false });

    await expect(tools.aft_search.execute({ topK: 3 } as never, sdkCtx)).rejects.toThrow(
      /missing field `query`|invalid params/i,
    );
  });

  test("aft_search with a valid query returns formatted text", async () => {
    const { tools, sdkCtx } = await createToolHarness({ experimentalSemanticSearch: true });

    const output = await tools.aft_search.execute(
      { query: "request authentication handler" },
      sdkCtx,
    );

    expect(typeof output).toBe("string");
    expect(output.length).toBeGreaterThan(0);

    // In CI without ONNX Runtime, various non-ready responses are valid.
    // Only assert structure when the index is actually ready.
    const isBuilding =
      output.includes("building") || output.includes("not ready") || output.includes("not_ready");
    const isUnavailable =
      output.includes("unavailable") ||
      output.includes("ONNX") ||
      output.includes("not found") ||
      output.includes("not enabled");
    const isDisabled = output.includes("disabled") || output.includes("not enabled");
    if (isBuilding || isUnavailable || isDisabled) {
      // Any non-ready state is acceptable in test environments
      expect(output.length).toBeGreaterThan(0);
    } else {
      expect(output).toContain("Found ");
      expect(output).toContain("[index: ready]");
      expect(output).toContain("src/");
    }
  });
});

async function createFixtureProject(root: string): Promise<void> {
  await mkdir(join(root, "src"), { recursive: true });
  await Promise.all([
    writeFile(
      join(root, "src", "lib.rs"),
      [
        "pub fn handle_request(token: &str) -> bool {",
        "  !token.is_empty()",
        "}",
        "",
        "pub struct AuthService;",
        "",
      ].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "src", "utils.rs"),
      [
        "pub fn normalize_user_id(input: &str) -> String {",
        "  input.trim().to_lowercase()",
        "}",
        "",
      ].join("\n"),
      "utf8",
    ),
  ]);
}
