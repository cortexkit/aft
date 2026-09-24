import { describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
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
  const result = event.result;
  if (result && typeof result === "object" && !Array.isArray(result)) {
    const content = (result as { content?: unknown }).content;
    if (Array.isArray(content)) {
      return content
        .map((part) => {
          if (!part || typeof part !== "object" || Array.isArray(part)) return "";
          const text = (part as { text?: unknown }).text;
          return typeof text === "string" ? text : "";
        })
        .filter((text) => text.length > 0)
        .join("\n");
    }
  }
  return JSON.stringify(result ?? "");
}

async function enableZoomSurface(env: PiIsolatedEnv): Promise<void> {
  await mkdir(join(env.workdir, ".pi"), { recursive: true });
  await writeFile(
    join(env.workdir, ".pi", "aft.jsonc"),
    JSON.stringify({
      indexes: { semantic: false },
      url_fetch_allow_private: true,
    }),
    "utf8",
  );
}

async function startMarkdownServer(): Promise<{ server: Server; url: string }> {
  const markdown = [
    "# Test Document",
    "",
    "## Section A",
    "",
    "Body of section A.",
    "",
    "## Section B",
    "",
    "Body of section B.",
    "",
  ].join("\n");
  const server = createServer((req, res) => {
    if ((req.url ?? "").split("?")[0] === "/doc.md") {
      res.writeHead(200, { "content-type": "text/markdown; charset=utf-8" });
      res.end(markdown);
      return;
    }
    res.writeHead(404, { "content-type": "text/plain" });
    res.end("not found");
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address() as AddressInfo;
  return { server, url: `http://127.0.0.1:${address.port}/doc.md` };
}

async function nextZoomStart(
  client: RpcClient,
  seenToolCallIds: Set<unknown>,
): Promise<Record<string, unknown>> {
  const event = await client.waitForEvent(
    (candidate) =>
      candidate.type === "tool_execution_start" &&
      candidate.toolName === "aft_zoom" &&
      !seenToolCallIds.has(candidate.toolCallId),
    60_000,
  );
  seenToolCallIds.add(event.toolCallId);
  return event;
}

describe("AFT Pi zoom tool (real Pi RPC)", () => {
  test("aft_zoom routes all shapes through server tool_call text", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    const { server, url } = await startMarkdownServer();
    let client: RpcClient | undefined;
    try {
      await enableZoomSurface(env);
      await writeFile(
        join(env.workdir, "zoom.ts"),
        [
          "export function alpha(): string {",
          "  return 'alpha';",
          "}",
          "",
          "export function beta(): string {",
          "  return alpha().toUpperCase();",
          "}",
          "",
        ].join("\n"),
        "utf8",
      );
      await mkdir(join(env.workdir, "src"), { recursive: true });
      await writeFile(join(env.workdir, "src", "a.ts"), "export function one() { return 1; }\n");
      await writeFile(join(env.workdir, "src", "b.ts"), "export function two() { return 2; }\n");

      aimock.registerToolCallFixture({
        predicate: () => true,
        toolCalls: [
          { name: "aft_zoom", arguments: { filePath: "zoom.ts", symbols: "alpha" } },
          { name: "aft_zoom", arguments: { filePath: "zoom.ts", symbols: ["alpha", "beta"] } },
          {
            name: "aft_zoom",
            arguments: { filePath: "zoom.ts", symbols: ["alpha", "missing"] },
          },
          { name: "aft_zoom", arguments: { url, symbols: "Section A" } },
          {
            name: "aft_zoom",
            arguments: {
              targets: [
                { filePath: "src/a.ts", symbol: "one" },
                { filePath: "src/b.ts", symbol: "two" },
              ],
            },
          },
          {
            name: "aft_zoom",
            arguments: {
              targets: [
                { filePath: "src/a.ts", symbol: "one" },
                { filePath: "src/b.ts", symbol: "missing" },
              ],
            },
          },
          { name: "aft_zoom", arguments: { url, symbols: ["Section A", "Section B"] } },
        ],
        followupText: "Done.",
      });

      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
        aftConfigOverrides: { url_fetch_allow_private: true },
      });
      client = spawned.client;
      expect(spawned.child.pid).toBeGreaterThan(0);
      expect(
        (await client.sendCommand({ type: "prompt", message: "Run the zoom checks." })).success,
      ).toBe(true);

      const seenStarts = new Set<unknown>();
      const starts = [
        await nextZoomStart(client, seenStarts),
        await nextZoomStart(client, seenStarts),
        await nextZoomStart(client, seenStarts),
        await nextZoomStart(client, seenStarts),
        await nextZoomStart(client, seenStarts),
        await nextZoomStart(client, seenStarts),
        await nextZoomStart(client, seenStarts),
      ];
      const [single, multi, sameFilePartial, urlSingle, targetsAll, targetsPartial, multiHeading] =
        await Promise.all(
          starts.map((start) =>
            client.waitForEvent(
              (event) =>
                event.type === "tool_execution_end" && event.toolCallId === start.toolCallId,
              60_000,
            ),
          ),
        );

      expect(single.isError).toBe(false);
      expect(resultText(single)).toContain("alpha");

      expect(multi.isError).toBe(false);
      expect(resultText(multi)).toContain("alpha");
      expect(resultText(multi)).toContain("beta");

      expect(sameFilePartial.isError).toBe(false);
      expect(resultText(sameFilePartial)).toContain("Incomplete zoom results");
      expect(resultText(sameFilePartial)).toContain('Symbol "missing" not found');

      expect(urlSingle.isError).toBe(false);
      expect(resultText(urlSingle)).toContain("Body of section A.");

      expect(targetsAll.isError).toBe(false);
      expect(resultText(targetsAll)).toContain("src/a.ts:1-1 [function one]");
      expect(resultText(targetsAll)).toContain("src/b.ts:1-1 [function two]");

      expect(targetsPartial.isError).toBe(false);
      expect(resultText(targetsPartial)).toContain("Incomplete zoom results");
      expect(resultText(targetsPartial)).toContain('Symbol "missing" not found in src/b.ts');

      expect(multiHeading.isError).toBe(false);
      expect(resultText(multiHeading)).toContain("Body of section A.");
      expect(resultText(multiHeading)).toContain("Body of section B.");
    } finally {
      await client?.close();
      await aimock.close();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      await cleanupPiIsolatedEnv(env);
    }
  }, 120_000);
});
