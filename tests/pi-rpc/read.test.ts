import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  type AimockHandle,
  cleanupPiIsolatedEnv,
  createPiIsolatedEnv,
  type PiIsolatedEnv,
  type RpcClient,
  resolvePiPluginDir,
  spawnPiRpc,
  startAimock,
} from "./helpers";

describe("hoisted read tool (real Pi RPC)", () => {
  let env: PiIsolatedEnv;
  let aimock: AimockHandle;

  beforeAll(async () => {
    env = createPiIsolatedEnv();
    aimock = await startAimock();
    aimock.registerToolCallFixture({
      predicate: () => true,
      toolCalls: [{ name: "read", arguments: { path: "hello.txt" } }],
      followupText: "I read the file.",
    });
    await writeFile(join(env.workdir, "hello.txt"), "Hello from AFT-Pi RPC E2E\n");
  });

  afterAll(async () => {
    await aimock.close();
    await cleanupPiIsolatedEnv(env);
  });

  test("Pi loads AFT plugin and dispatches hoisted read via RPC", async () => {
    let client: RpcClient | undefined;
    try {
      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;

      const promptResp = await client.sendCommand({
        type: "prompt",
        message: "Read hello.txt and tell me what's in it.",
      });
      expect(promptResp.success).toBe(true);
      expect(spawned.child.pid).toBeGreaterThan(0);

      const toolStart = await client.waitForEvent(
        (event) => event.type === "tool_execution_start" && event.toolName === "read",
        30_000,
      );
      expect(toolStart.args).toMatchObject({ path: "hello.txt" });

      const toolEnd = await client.waitForEvent(
        (event) => event.type === "tool_execution_end" && event.toolCallId === toolStart.toolCallId,
        30_000,
      );
      expect(toolEnd.isError).toBe(false);
      expect(JSON.stringify(toolEnd.result)).toContain("Hello from AFT-Pi RPC E2E");

      await client.waitForEvent((event) => event.type === "agent_end", 30_000);
    } finally {
      await client?.close();
    }
  }, 120_000);
});
