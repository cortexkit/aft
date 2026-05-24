/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { chmod, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
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
    sessionID: "apply-patch-rollback-e2e",
    messageID: "apply-patch-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

maybeDescribe("e2e apply_patch rollback behavior", () => {
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

  async function toolHarness(): Promise<{
    h: E2EHarness;
    tools: ReturnType<typeof hoistedTools>;
    sdkCtx: ToolContext;
  }> {
    const h = await createHarness(preparedBinary, { fixtureNames: [], timeoutMs: 20_000 });
    harnesses.push(h);
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      { storage_dir: join(h.tempDir, ".storage"), harness: "opencode" },
    );
    pools.push(pool);
    return {
      h,
      tools: hoistedTools(createPluginContext(pool, join(h.tempDir, ".storage"))),
      sdkCtx: createSdkContext(h.tempDir),
    };
  }

  test("successful two-file patch updates both files", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("one.txt"), "alpha\n", "utf8");
    await writeFile(h.path("two.txt"), "bravo\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Update File: one.txt
@@
-alpha
+alpha changed
*** Update File: two.txt
@@
-bravo
+bravo changed
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Updated one.txt");
    expect(output).toContain("Updated two.txt");
    expect(await readTextFile(h.path("one.txt"))).toBe("alpha changed\n");
    expect(await readTextFile(h.path("two.txt"))).toBe("bravo changed\n");
  });

  test("move hunk success removes source and writes destination", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("from.txt"), "move me\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Update File: from.txt
*** Move to: nested/to.txt
@@
-move me
+move me please
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Updated and moved from.txt → nested/to.txt");
    await expect(readTextFile(h.path("from.txt"))).rejects.toThrow();
    expect(await readTextFile(h.path("nested", "to.txt"))).toBe("move me please\n");
  });

  test("move source-delete failure restores source and removes new destination", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await mkdir(h.path("locked"), { recursive: true });
    await writeFile(h.path("locked", "from.txt"), "original\n", "utf8");
    await chmod(h.path("locked"), 0o555);

    try {
      // Total-failure (single hunk that fully fails) now throws so OpenCode
      // marks the tool call as errored in the UI. Capture the thrown error
      // to verify both the failure summary and the rollback messaging are
      // preserved in the error message.
      let caught: unknown;
      try {
        await tools.apply_patch.execute(
          {
            patchText: `*** Begin Patch
*** Update File: locked/from.txt
*** Move to: moved/to.txt
@@
-original
+changed
*** End Patch`,
          },
          sdkCtx,
        );
      } catch (e) {
        caught = e;
      }
      expect(caught).toBeInstanceOf(Error);
      const message = (caught as Error).message;
      expect(message).toContain("Failed to update locked/from.txt");
      expect(message).toContain("source delete failed after writing move destination");
      expect(message).toContain("Patch failed");
      expect(await readTextFile(h.path("locked", "from.txt"))).toBe("original\n");
      await expect(readTextFile(h.path("moved", "to.txt"))).rejects.toThrow();
    } finally {
      await chmod(h.path("locked"), 0o755).catch(() => {});
    }
  });

  test("add hunk to existing path should fail without rolling back unrelated successful file", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("kept.txt"), "before\n", "utf8");
    await writeFile(h.path("exists.txt"), "already here\n", "utf8");

    const output = await tools.apply_patch.execute(
      {
        patchText: `*** Begin Patch
*** Update File: kept.txt
@@
-before
+after
*** Add File: exists.txt
+new content
*** End Patch`,
      },
      sdkCtx,
    );

    expect(output).toContain("Updated kept.txt");
    expect(output).toContain("Failed to create exists.txt");
    expect(output).toContain("Patch partially applied");
    expect(await readTextFile(h.path("kept.txt"))).toBe("after\n");
    expect(await readTextFile(h.path("exists.txt"))).toBe("already here\n");
  });
});
