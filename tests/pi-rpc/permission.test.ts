import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
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

describe("extension UI permission ask (real Pi RPC)", () => {
  let env: PiIsolatedEnv;
  let aimock: AimockHandle;
  let outsideDir: string;

  beforeAll(async () => {
    env = createPiIsolatedEnv();
    aimock = await startAimock();
    outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
  });

  afterAll(async () => {
    await aimock.close();
    await cleanupPiIsolatedEnv(env);
    await rm(outsideDir, { recursive: true, force: true });
  });

  test("Pi handles extension_ui_request for external-directory permission ask", async () => {
    const outOfProjectFile = join(outsideDir, "outside-project-test.txt");
    await writeFile(outOfProjectFile, "original content\n");
    aimock.registerToolCallFixture({
      predicate: () => true,
      toolCalls: [
        {
          name: "edit",
          arguments: {
            filePath: outOfProjectFile,
            oldString: "original",
            newString: "modified",
          },
        },
      ],
      followupText: "Edit complete.",
    });

    let client: RpcClient | undefined;
    try {
      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;

      let uiRequestSeen = false;
      client.onExtensionUIRequest((request) => {
        uiRequestSeen = true;
        expect(["confirm", "select"]).toContain(String(request.method));
        client?.sendExtensionUIResponse({
          id: request.id as string,
          cancelled: true,
        });
      });

      const promptResp = await client.sendCommand({
        type: "prompt",
        message: `Edit ${outOfProjectFile} to change 'original' to 'modified'.`,
      });
      expect(promptResp.success).toBe(true);
      expect(spawned.child.pid).toBeGreaterThan(0);

      const toolEnd = await client.waitForEvent(
        (event) => event.type === "tool_execution_end" && event.toolName === "edit",
        30_000,
      );
      expect(uiRequestSeen).toBe(true);
      expect(toolEnd.isError).toBe(true);
      expect(JSON.stringify(toolEnd.result).toLowerCase()).toMatch(/permission|denied|cancelled/);

      const after = await readFile(outOfProjectFile, "utf8");
      expect(after).toBe("original content\n");
    } finally {
      await client?.close();
    }
  }, 120_000);
});
