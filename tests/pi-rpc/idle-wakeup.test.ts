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

function eventText(event: Record<string, unknown>): string {
  return JSON.stringify(event);
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

async function enableAftBash(workdir: string): Promise<void> {
  await mkdir(join(workdir, ".pi"), { recursive: true });
  await writeFile(
    join(workdir, ".pi", "aft.jsonc"),
    JSON.stringify({ experimental: { bash: { background: true, compress: true } } }),
  );
}

describe("background bash idle wakeup (real Pi RPC)", () => {
  test("completion after agent_end is delivered as a follow-up user message event", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    let client: RpcClient | undefined;
    try {
      await enableAftBash(env.workdir);
      aimock.registerToolCallFixture({
        predicate: (request) => latestUserText(request).includes("Start idle background bash."),
        toolCalls: [
          { name: "bash", arguments: { command: "sleep 3; echo idle-done", background: true } },
        ],
        followupText: "The background task is running.",
      });
      aimock.registerTextFixture({
        predicate: (request) => latestUserText(request).includes("[BACKGROUND BASH COMPLETED]"),
        content: "Noted the background completion.",
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
        (await client.sendCommand({ type: "prompt", message: "Start idle background bash." }))
          .success,
      ).toBe(true);
      await client.waitForEvent((event) => event.type === "agent_end", 30_000);

      // Pi's RPC stream currently surfaces the idle wake as a normal
      // message_start/message_end pair whose message role is "user"; the
      // reminder body is the follow-up payload queued by the runtime.
      const wakeEvent = await client.waitForEvent(
        (event) =>
          (event.type === "message_start" || event.type === "message_end") &&
          eventText(event).includes('"role":"user"') &&
          eventText(event).includes("[BACKGROUND BASH COMPLETED]") &&
          eventText(event).includes("idle-done"),
        10_000,
      );
      expect(wakeEvent.type).toBe("message_start");
    } finally {
      await client?.close();
      await aimock.close();
      await cleanupPiIsolatedEnv(env);
    }
  }, 120_000);
});
