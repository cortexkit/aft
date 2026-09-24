/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { ToolContext } from "@opencode-ai/plugin";
import type { PluginContext } from "../../types.js";
import { createReadTool } from "../hoisted.js";

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
      return { success: true, text: "read result" };
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

Use either startLine/endLine OR offset/limit to read a section of a file.

Behavior:
- Returns line-numbered content (e.g., "1: const x = 1")
- Lines longer than 2000 characters are truncated
- Output capped at 50KB
- Binary files are auto-detected and return a size-only message
- Supported images (PNG, JPEG, GIF, WebP) and PDFs are returned as tool attachments; range arguments are ignored for media
- Directories return sorted entries with trailing / for subdirectories

Examples:
  Read full file: { "path": "src/app.ts" }
  Read lines 50-100: { "path": "src/app.ts", "startLine": 50, "endLine": 100 }
  Read 30 lines from line 200: { "path": "src/app.ts", "offset": 200, "limit": 30 }
  List directory: { "path": "src/" }
`);
  });

  test("advertises GitHub resource spellings only when the user gate is enabled", () => {
    const enabled = makeHarness([{ id: "text" }], true).tool.description;
    const disabled = makeHarness([{ id: "text" }], false).tool.description;

    expect(enabled).toContain(
      "GitHub issues and pull requests can be read with `issue://NUMBER` and `pr://NUMBER`",
    );
    expect(disabled).not.toContain("issue://NUMBER");
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

  test("reads the session model again on every call so a model switch takes effect", async () => {
    const harness = makeHarness([
      { id: "vision", modalities: { input: ["text", "image"] } },
      { id: "text", modalities: { input: ["text"] } },
    ]);

    await read(harness.tool, harness.context);
    harness.setCurrentModelID("text");
    await read(harness.tool, harness.context);

    expect(harness.calls.map((call) => call.args.vision_capability)).toEqual([true, false]);
    expect(harness.lookupCounts()).toEqual({ messageLookups: 2, providerLookups: 2 });
  });
});
