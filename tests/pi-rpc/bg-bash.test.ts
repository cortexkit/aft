import { describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  cleanupPiIsolatedEnv,
  createPiIsolatedEnv,
  type RpcClient,
  resolvePiPluginDir,
  spawnPiRpc,
  startAimock,
} from "./helpers";

const BASH_TASK_ID = /^bash-[a-f0-9]{8}$/;

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

function latestUserText(request: unknown): string {
  const messages = (request as { messages?: unknown[] } | undefined)?.messages ?? [];
  const latest = [...messages]
    .reverse()
    .find(
      (message): message is { role: string; content?: unknown } =>
        !!message && typeof message === "object" && (message as { role?: unknown }).role === "user",
    );
  const content = latest?.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part) =>
      part && typeof part === "object" && (part as { type?: unknown }).type === "text"
        ? String((part as { text?: unknown }).text ?? "")
        : "",
    )
    .join("\n");
}

async function enableAftBash(env: { workdir: string }): Promise<void> {
  await mkdir(join(env.workdir, ".pi"), { recursive: true });
  await writeFile(
    join(env.workdir, ".pi", "aft.jsonc"),
    JSON.stringify({ experimental: { bash: { background: true, compress: true } } }),
  );
}

async function pollBashStatus(client: RpcClient, taskId: string): Promise<Record<string, unknown>> {
  // 90s overall deadline + 60s per-poll wait: under load (cold Pi spawn,
  // ONNX init, LLM round-trip on shared macOS runners) a single
  // bash_status round-trip can take 20-40s. The previous 30s deadline
  // was tight enough that one slow LLM turn would fail the whole test.
  const deadline = Date.now() + 90_000;
  let attempt = 0;
  const seenToolCallIds = new Set<unknown>();
  while (Date.now() < deadline) {
    attempt += 1;
    const prompt = `Check background bash task ${taskId}, attempt ${attempt}.`;
    expect((await client.sendCommand({ type: "prompt", message: prompt })).success).toBe(true);
    const statusEnd = await client.waitForEvent(
      (event) =>
        event.type === "tool_execution_end" &&
        event.toolName === "bash_status" &&
        !seenToolCallIds.has(event.toolCallId) &&
        resultText(event).includes(taskId),
      60_000,
    );
    seenToolCallIds.add(statusEnd.toolCallId);
    if (resultDetails(statusEnd).status === "completed") return statusEnd;
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(`Timed out polling ${taskId} to completed`);
}

describe("background bash lifecycle (real Pi RPC)", () => {
  test("background spawn returns a bash slug and bash_status reaches completed", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    let client: RpcClient | undefined;
    try {
      await enableAftBash(env);
      aimock.registerToolCallFixture({
        predicate: (request) => latestUserText(request).includes("Start a background echo."),
        toolCalls: [{ name: "bash", arguments: { command: "echo hello", background: true } }],
        followupText: "Started.",
      });
      aimock.registerTextFixture({
        predicate: (request) =>
          latestUserText(request).includes("[BACKGROUND BASH COMPLETED]") &&
          !latestUserText(request).includes("Check background bash task"),
        content: "Noted.",
      });

      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;
      expect(spawned.child.pid).toBeGreaterThan(0);

      expect(
        (await client.sendCommand({ type: "prompt", message: "Start a background echo." })).success,
      ).toBe(true);
      // 90s budgets: Pi cold start + ONNX init + first LLM call + bg-bash
      // spawn can exceed 60s on shared macOS CI runners under load. The
      // previous 60s budget tripped at 61256ms in the v0.27 release CI run
      // because the first LLM call + bridge spawn alone consumed it. The
      // outer test budget is raised in lockstep below.
      const bashEnd = await client.waitForEvent(
        (event) => event.type === "tool_execution_end" && event.toolName === "bash",
        90_000,
      );
      expect(bashEnd.isError).toBe(false);
      const taskId = resultDetails(bashEnd).task_id;
      expect(taskId).toEqual(expect.stringMatching(BASH_TASK_ID));
      expect(resultText(bashEnd)).toContain(`Background task started: ${taskId}`);
      await client.waitForEvent((event) => event.type === "agent_end", 60_000);

      aimock.registerToolCallFixture({
        predicate: (request) =>
          latestUserText(request).includes(`Check background bash task ${taskId}`),
        toolCalls: [{ name: "bash_status", arguments: { task_id: taskId } }],
        followupText: "Checked.",
      });
      const statusEnd = await pollBashStatus(client, String(taskId));
      expect(statusEnd.isError).toBe(false);
      expect(resultDetails(statusEnd)).toMatchObject({ status: "completed", exit_code: 0 });
    } finally {
      await client?.close();
      await aimock.close();
      await cleanupPiIsolatedEnv(env);
    }
  }, 180_000);

  test("background completion is appended to the next unrelated tool result", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    let client: RpcClient | undefined;
    try {
      await enableAftBash(env);
      aimock.registerToolCallFixture({
        predicate: (request) => latestUserText(request).includes("Start background before read."),
        toolCalls: [
          {
            name: "bash",
            arguments: { command: "echo bg-done", background: true },
          },
        ],
        followupText: "Started.",
      });
      aimock.registerToolCallFixture({
        predicate: (request) => latestUserText(request).includes("Read anchor after background."),
        toolCalls: [{ name: "read", arguments: { path: "anchor.txt" } }],
        followupText: "Read.",
      });
      await writeFile(join(env.workdir, "anchor.txt"), "keeps the project non-empty\n");

      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;

      expect(
        (
          await client.sendCommand({
            type: "prompt",
            message: "Start background before read.",
          })
        ).success,
      ).toBe(true);
      const bgEnd = await client.waitForEvent(
        (event) =>
          event.type === "tool_execution_end" &&
          event.toolName === "bash" &&
          resultText(event).includes("Background task started"),
        30_000,
      );
      const taskId = String(resultDetails(bgEnd).task_id);
      expect(taskId).toEqual(expect.stringMatching(BASH_TASK_ID));
      await client.waitForEvent((event) => event.type === "agent_end", 30_000);
      await new Promise((resolve) => setTimeout(resolve, 250));

      expect(
        (
          await client.sendCommand({
            type: "prompt",
            message: "Read anchor after background.",
          })
        ).success,
      ).toBe(true);

      const readEnd = await client.waitForEvent(
        (event) =>
          event.type === "tool_execution_end" &&
          event.toolName === "read" &&
          resultText(event).includes("keeps the project non-empty"),
        30_000,
      );
      expect(readEnd.isError).toBe(false);
      const reminder = await client.waitForEvent(
        (event) =>
          (event.type === "message_start" || event.type === "message_end") &&
          JSON.stringify(event).includes("[BACKGROUND BASH COMPLETED]") &&
          JSON.stringify(event).includes(taskId) &&
          JSON.stringify(event).includes("bg-done"),
        30_000,
      );
      expect(reminder.type).toBe("message_start");
    } finally {
      await client?.close();
      await aimock.close();
      await cleanupPiIsolatedEnv(env);
    }
  }, 120_000);
});
