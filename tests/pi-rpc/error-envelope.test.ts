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
    /** Tool arguments computed after `setup`, replacing `toolCall.arguments`. */
    toolArguments?: () => Record<string, unknown>;
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
      toolCalls: [opts.toolArguments ? { ...toolCall, arguments: opts.toolArguments() } : toolCall],
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
    // The file is inside the project, so no outside-access approval is
    // involved and the read reaches the not-found error.
    const toolEnd = await withPiTool(
      { name: "read", arguments: { path: "missing.txt" } },
      { message: "Read missing.txt." },
    );
    expect(toolEnd.isError).toBe(true);
    expect(resultText(toolEnd).toLowerCase()).toMatch(/not.*found|enoent/);
  }, 120_000);

  // Pi's RPC mode is never asked about outside access (only Pi's own terminal
  // UI is), so outside access is allowed without a prompt, as before AFT asked.
  // The engine's floors still refuse, and nothing waits on the RPC client.
  function watchRead(client: RpcClient, seen: { confirm: boolean; start: number; end: number }) {
    client.onEvent((event) => {
      if (event.toolName !== "read") return;
      if (event.type === "tool_execution_start") seen.start = Date.now();
      if (event.type === "tool_execution_end") seen.end = Date.now();
    });
    client.onExtensionUIRequest((request) => {
      if (request.method === "confirm") seen.confirm = true;
    });
  }

  test("read of an existing file outside the project succeeds without a prompt", async () => {
    const seen = { confirm: false, start: 0, end: 0 };
    let outsideFile = "";
    const toolEnd = await withPiTool(
      // The path is filled in once the isolated environment exists.
      { name: "read", arguments: { path: "" } },
      {
        message: "Read the outside notes.",
        setup: async (env) => {
          // Beside the project, and outside the system temp folder, so the
          // engine treats it as outside access rather than scratch.
          const outside = join(env.baseDir, "outside");
          await mkdir(outside, { recursive: true });
          outsideFile = join(outside, "notes.txt");
          await writeFile(outsideFile, "outside notes 4b2e\n");
        },
        toolArguments: () => ({ path: outsideFile }),
        onClient: (client) => watchRead(client, seen),
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(resultText(toolEnd)).toContain("outside notes 4b2e");
    expect(seen.confirm).toBe(false);
  }, 120_000);

  test("read of a nonexistent outside path gives the not-found result without a prompt", async () => {
    const seen = { confirm: false, start: 0, end: 0 };
    const toolEnd = await withPiTool(
      { name: "read", arguments: { path: "/does/not/exist" } },
      {
        message: "Read /does/not/exist.",
        onClient: (client) => watchRead(client, seen),
      },
    );
    expect(toolEnd.isError).toBe(true);
    expect(resultText(toolEnd).toLowerCase()).toMatch(/not.*found|enoent/);
    expect(seen.confirm).toBe(false);
    expect(seen.start).toBeGreaterThan(0);
    expect(seen.end - seen.start).toBeLessThan(5_000);
  }, 120_000);

  test("read of a floor-protected path is still refused, without a prompt", async () => {
    const seen = { confirm: false, start: 0, end: 0 };
    let protectedFile = "";
    const toolEnd = await withPiTool(
      { name: "read", arguments: { path: "" } },
      {
        message: "Read the CortexKit data file.",
        setup: async (env) => {
          // Pi runs with HOME set to the environment's config folder, so
          // this is `~/.local/share/cortexkit` for the session. File tools may
          // read ordinary files there, but never a module's database file.
          const store = join(env.configDir, ".local", "share", "cortexkit");
          await mkdir(store, { recursive: true });
          protectedFile = join(store, "probe.db");
          await writeFile(protectedFile, "protected body 9d1a\n");
        },
        toolArguments: () => ({ path: protectedFile }),
        onClient: (client) => watchRead(client, seen),
      },
    );
    expect(toolEnd.isError).toBe(true);
    expect(resultText(toolEnd)).not.toContain("protected body 9d1a");
    expect(seen.confirm).toBe(false);
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
