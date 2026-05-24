import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  cleanupPiIsolatedEnv,
  createPiIsolatedEnv,
  type PiIsolatedEnv,
  type RpcClient,
  resolvePiPluginDir,
  spawnPiRpc,
  startAimock,
} from "./helpers";

async function withPiTool(
  toolCall: { name: string; arguments: Record<string, unknown> },
  opts: {
    message: string;
    setup?: (env: PiIsolatedEnv) => Promise<void>;
    onClient?: (client: RpcClient) => void;
    afterTool?: (env: PiIsolatedEnv, toolEnd: Record<string, unknown>) => Promise<void>;
  },
) {
  const env = createPiIsolatedEnv();
  const aimock = await startAimock();
  let client: RpcClient | undefined;
  try {
    await opts.setup?.(env);
    aimock.registerToolCallFixture({
      predicate: () => true,
      toolCalls: [toolCall],
      followupText: "Done.",
    });
    const spawned = spawnPiRpc({
      mockProviderURL: aimock.url,
      aftPluginDir: resolvePiPluginDir(),
      configDir: env.configDir,
      workdir: env.workdir,
    });
    client = spawned.client;
    opts.onClient?.(client);
    expect((await client.sendCommand({ type: "prompt", message: opts.message })).success).toBe(
      true,
    );
    const toolEnd = await client.waitForEvent(
      (event) => event.type === "tool_execution_end" && event.toolName === toolCall.name,
      30_000,
    );
    await opts.afterTool?.(env, toolEnd);
    return toolEnd;
  } finally {
    await client?.close();
    await aimock.close();
    await cleanupPiIsolatedEnv(env);
  }
}

describe("permission matrix (real Pi RPC)", () => {
  test("project-internal edit does not ask for external-directory permission", async () => {
    let uiRequestSeen = false;
    const toolEnd = await withPiTool(
      {
        name: "edit",
        arguments: { filePath: "inside.txt", oldString: "before", newString: "after" },
      },
      {
        message: "Edit the project file inside.txt.",
        setup: async (env) => writeFile(join(env.workdir, "inside.txt"), "before content\n"),
        onClient: (client) => {
          client.onExtensionUIRequest(() => {
            uiRequestSeen = true;
          });
        },
        afterTool: async (env) => {
          expect(await readFile(join(env.workdir, "inside.txt"), "utf8")).toBe("after content\n");
        },
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(uiRequestSeen).toBe(false);
    expect(JSON.stringify(toolEnd.result)).toContain("Edited inside.txt");
  }, 120_000);

  test("external edit succeeds when permission is confirmed", async () => {
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
    try {
      const target = join(outsideDir, "confirmed-edit.txt");
      await writeFile(target, "original content\n");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        {
          name: "edit",
          arguments: { filePath: target, oldString: "original", newString: "modified" },
        },
        {
          message: `Edit ${target}.`,
          onClient: (client) => {
            client.onExtensionUIRequest((request) => {
              uiRequestSeen = true;
              client.sendExtensionUIResponse({ id: request.id as string, confirmed: true });
            });
          },
        },
      );
      expect(uiRequestSeen).toBe(true);
      expect(toolEnd.isError).toBe(false);
      expect(await readFile(target, "utf8")).toBe("modified content\n");
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);

  test("external write cancellation returns an error and leaves file absent", async () => {
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
    try {
      const target = join(outsideDir, "cancelled-write.txt");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        { name: "write", arguments: { filePath: target, content: "new content\n" } },
        {
          message: `Write ${target}.`,
          onClient: (client) => {
            client.onExtensionUIRequest((request) => {
              uiRequestSeen = true;
              client.sendExtensionUIResponse({ id: request.id as string, cancelled: true });
            });
          },
        },
      );
      expect(uiRequestSeen).toBe(true);
      expect(toolEnd.isError).toBe(true);
      expect(JSON.stringify(toolEnd.result).toLowerCase()).toMatch(/permission|denied|cancelled/);
      expect(existsSync(target)).toBe(false);
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);

  test("external grep cancellation returns an error envelope", async () => {
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
    try {
      await writeFile(join(outsideDir, "search.txt"), "needle\n");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        { name: "grep", arguments: { pattern: "needle", path: outsideDir } },
        {
          message: `Search ${outsideDir}.`,
          onClient: (client) => {
            client.onExtensionUIRequest((request) => {
              uiRequestSeen = true;
              client.sendExtensionUIResponse({ id: request.id as string, cancelled: true });
            });
          },
        },
      );
      expect(uiRequestSeen).toBe(true);
      expect(toolEnd.isError).toBe(true);
      expect(JSON.stringify(toolEnd.result).toLowerCase()).toMatch(/permission|denied|cancelled/);
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);
});
