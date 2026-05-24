import { describe, expect, test } from "bun:test";
import { mkdir, readFile, writeFile } from "node:fs/promises";
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

function resultText(event: Record<string, unknown>): string {
  return JSON.stringify(event.result ?? "");
}

function resultDetails(event: Record<string, unknown>): Record<string, unknown> {
  const result = event.result;
  if (!result || typeof result !== "object" || Array.isArray(result)) return {};
  const details = (result as Record<string, unknown>).details;
  return details && typeof details === "object" && !Array.isArray(details)
    ? (details as Record<string, unknown>)
    : {};
}

async function enableAftBash(env: PiIsolatedEnv): Promise<void> {
  await mkdir(join(env.workdir, ".pi"), { recursive: true });
  await writeFile(
    join(env.workdir, ".pi", "aft.jsonc"),
    JSON.stringify({ experimental: { bash: { background: true, compress: true } } }),
  );
}

async function withPiTool(
  toolCall: { name: string; arguments: Record<string, unknown> },
  opts: {
    message: string;
    setup?: (env: PiIsolatedEnv) => Promise<void>;
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
    expect(spawned.child.pid).toBeGreaterThan(0);
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

describe("tool error envelopes (real Pi RPC)", () => {
  test("read missing file returns an error envelope", async () => {
    const toolEnd = await withPiTool(
      { name: "read", arguments: { path: "/does/not/exist" } },
      { message: "Read /does/not/exist." },
    );
    expect(toolEnd.isError).toBe(true);
    expect(resultText(toolEnd).toLowerCase()).toMatch(/not.*found|enoent/);
  }, 120_000);

  test("edit with no matching oldString errors and leaves file unchanged", async () => {
    const toolEnd = await withPiTool(
      {
        name: "edit",
        arguments: { filePath: "unchanged.txt", oldString: "missing", newString: "changed" },
      },
      {
        message: "Try an edit that should not match.",
        setup: async (env) => writeFile(join(env.workdir, "unchanged.txt"), "original\n"),
        afterTool: async (env) => {
          expect(await readFile(join(env.workdir, "unchanged.txt"), "utf8")).toBe("original\n");
        },
      },
    );
    expect(toolEnd.isError).toBe(true);
    expect(resultText(toolEnd).toLowerCase()).toMatch(/no match|not found|matched 0/);
  }, 120_000);

  test("bash non-zero exit is a successful tool call with exit_code 1", async () => {
    const toolEnd = await withPiTool(
      { name: "bash", arguments: { command: "exit 1" } },
      {
        message: "Run a command that exits non-zero.",
        setup: enableAftBash,
      },
    );
    // The shell process failed, but the bash tool itself succeeded at running
    // it, so Pi should not wrap this in an isError=true envelope.
    expect(toolEnd.isError).toBe(false);
    expect(resultDetails(toolEnd).exit_code).toBe(1);
    expect(resultText(toolEnd)).toContain("exit_code");
  }, 120_000);
});
