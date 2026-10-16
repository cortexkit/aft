/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { ToolContext } from "@opencode-ai/plugin";
import { rememberReadModel, VISION_HOST_TIMEOUT_MS } from "../shared/read-vision.js";
import { createReadTool } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";

type RecordedCall = {
  sessionID: string | undefined;
  name: string;
  args: Record<string, unknown>;
};

type ModelConfig = {
  id: string;
  modalities?: { input?: string[] };
  attachment?: boolean;
};

function makeHarness(models: ModelConfig[], ghReadEnabled = false) {
  let currentModelID = models[0]?.id ?? "missing";
  const calls: RecordedCall[] = [];
  let messageLookups = 0;
  let providerLookups = 0;
  const bridge = {
    async toolCall(sessionID: string | undefined, name: string, args: Record<string, unknown>) {
      calls.push({ sessionID, name, args: { ...args } });
      return {
        success: true,
        text: "read result",
        attachments: [{ mime: "image/png", data: "AA==" }],
      };
    },
  };
  const client = {
    session: {
      async get() {
        return { data: { directory: process.cwd() } };
      },
      async messages() {
        messageLookups += 1;
        return {
          data: [
            {
              info: {
                role: "assistant",
                providerID: "test-provider",
                modelID: currentModelID,
              },
            },
          ],
        };
      },
    },
    provider: {
      async list() {
        providerLookups += 1;
        return {
          data: {
            all: [
              {
                id: "test-provider",
                models: Object.fromEntries(models.map((model) => [model.id, model])),
              },
            ],
          },
        };
      },
    },
  };
  const pluginContext = {
    pool: { getBridge: () => bridge },
    client,
    config: { github: { read: ghReadEnabled } },
    storageDir: "/tmp/aft-vision-capability",
  } as unknown as PluginContext;
  const tool = createReadTool(pluginContext);
  const context = {
    sessionID: `vision-${crypto.randomUUID()}`,
    messageID: "message",
    agent: "agent",
    directory: process.cwd(),
    worktree: process.cwd(),
    abort: new AbortController().signal,
    metadata: () => {},
    ask: async () => {},
  } as ToolContext;

  return {
    tool,
    client,
    context,
    calls,
    setCurrentModelID(modelID: string) {
      currentModelID = modelID;
    },
    lookupCounts() {
      return { messageLookups, providerLookups };
    },
  };
}

async function read(tool: ReturnType<typeof createReadTool>, context: ToolContext): Promise<void> {
  await tool.execute({ filePath: "issue://42" }, context);
}

describe("OpenCode read vision capability", () => {
  test("injects true for a vision-capable current model without exposing a schema field", async () => {
    const harness = makeHarness([{ id: "vision", modalities: { input: ["text", "image"] } }]);

    await read(harness.tool, harness.context);

    expect(harness.calls).toEqual([
      {
        sessionID: harness.context.sessionID,
        name: "read",
        args: { filePath: "issue://42", vision_capability: true },
      },
    ]);
    expect(Object.keys(harness.tool.args)).toEqual([
      "filePath",
      "startLine",
      "endLine",
      "limit",
      "offset",
    ]);
    expect(harness.tool.args).not.toHaveProperty("vision_capability");
    expect(harness.tool.description).toBe(`Read file contents or list directory entries.

Use either startLine/endLine OR offset/limit to read a section of a file or sorted directory listing.

Behavior:
- Returns line-numbered content (e.g., "1: const x = 1")
- Lines longer than 2000 characters are truncated
- Output capped at 50KB
- Binary files are auto-detected and return a size-only message
- Supported images (PNG, JPEG, GIF, WebP) and PDFs are returned as tool attachments; range arguments are ignored for media
- Directories return sorted entries with trailing / for subdirectories; offset is 1-based, limit defaults to and is capped at 1000 entries. Enumeration stops at 10,000 entries; partial listings carry a shown/total trailer.

Examples:
  Read full file: { "filePath": "src/app.ts" }
  Read lines 50-100: { "filePath": "src/app.ts", "startLine": 50, "endLine": 100 }
  Read 30 lines from line 200: { "filePath": "src/app.ts", "offset": 200, "limit": 30 }
  List directory: { "filePath": "src/" }
`);
  });

  test("advertises GitHub resource spellings only when the user gate is enabled", () => {
    const enabled = makeHarness([{ id: "text" }], true).tool.description;
    const disabled = makeHarness([{ id: "text" }], false).tool.description;

    expect(enabled).toContain(
      "GitHub issues and pull requests can be read with `issue://NUMBER` and `pr://NUMBER`",
    );
    expect(disabled).not.toContain("issue://NUMBER");
    expect(enabled).toContain("pr://NUMBER/diff/<path>");
    expect(disabled).not.toContain("/diff");
  });

  test("injects false for a vision-less current model", async () => {
    const harness = makeHarness([{ id: "text", modalities: { input: ["text"] } }]);

    await read(harness.tool, harness.context);

    expect(harness.calls[0]?.args).toEqual({ filePath: "issue://42", vision_capability: false });
  });

  test("omits the internal field when the current model has no capability data", async () => {
    const harness = makeHarness([{ id: "unknown" }]);

    await read(harness.tool, harness.context);

    expect(harness.calls[0]?.args).toEqual({ filePath: "issue://42" });
  });

  test("host model switch refreshes capability without history", async () => {
    const harness = makeHarness([
      { id: "vision", modalities: { input: ["text", "image"] } },
      { id: "text", modalities: { input: ["text"] } },
    ]);

    rememberReadModel(harness.client, harness.context.sessionID, {
      providerID: "test-provider",
      modelID: "vision",
    });
    await read(harness.tool, harness.context);
    rememberReadModel(harness.client, harness.context.sessionID, {
      providerID: "test-provider",
      modelID: "text",
    });
    await read(harness.tool, harness.context);

    expect(harness.calls.map((call) => call.args.vision_capability)).toEqual([true, false]);
    expect(harness.lookupCounts()).toEqual({ messageLookups: 0, providerLookups: 2 });
  });
});

test("concurrent image reads coalesce history and catalog and reuse capability", async () => {
  const harness = makeHarness([{ id: "vision", attachment: true }]);
  const messages = harness.client.session.messages;
  harness.client.session.messages = async () => {
    await new Promise((resolve) => setTimeout(resolve, 20));
    return messages();
  };
  const directory = await mkdtemp(join(process.cwd(), ".vision-test-"));
  const filePath = join(directory, "image.png");
  try {
    await writeFile(filePath, Buffer.from("iVBORw0KGgo=", "base64"));
    await Promise.all(
      Array.from({ length: 8 }, () => harness.tool.execute({ filePath }, harness.context)),
    );
    expect(harness.lookupCounts()).toEqual({ messageLookups: 1, providerLookups: 1 });
    expect(harness.calls.every((call) => call.args.vision_capability === true)).toBe(true);
    await harness.tool.execute({ filePath }, harness.context);
    expect(harness.lookupCounts().providerLookups).toBe(1);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

for (const host of ["history", "catalog"] as const) {
  test(`hung ${host} times out to safe unknown capability`, async () => {
    const harness = makeHarness([{ id: "vision", attachment: true }]);
    if (host === "history") harness.client.session.messages = () => new Promise(() => {});
    else harness.client.provider.list = () => new Promise(() => {});
    const start = performance.now();
    await read(harness.tool, harness.context);
    expect(performance.now() - start).toBeLessThan(VISION_HOST_TIMEOUT_MS + 1000);
    expect(harness.calls[0]?.args).toEqual({ filePath: "issue://42" });
  });
}
