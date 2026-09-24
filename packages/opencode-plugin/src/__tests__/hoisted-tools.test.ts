/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import type { BridgePool, ToolCallOptions } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { hoistedTools } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");
let sdkCtx = createMockSdkContext(PROJECT_CWD);
let tmpDir: string | null = null;

/**
 * read/write/edit/apply_patch now return `{ output, title, metadata }` so UI
 * metadata (title + diff) rides on the result instead of a side-channel store.
 * Most assertions only care about the agent-visible text — unwrap it here
 * (tolerating the legacy bare-string shape).
 */
function text(r: unknown): string {
  return typeof r === "string" ? r : ((r as { output?: string })?.output ?? "");
}

async function makeTempDir(): Promise<string> {
  return await realpath(await mkdtemp(resolve(tmpdir(), "aft-hoisted-")));
}

type BridgeResponse = Record<string, unknown>;
type SendCall = {
  command: string;
  params: Record<string, unknown>;
  options?: ToolCallOptions;
};

/** Creates a mock client that returns no connected LSP servers. */
function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
  };
}

/** Helper to create a PluginContext with a pool and a mock client. */
function createPluginContext(
  pool: BridgePool,
  config: PluginContext["config"] = {} as PluginContext["config"],
): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config,
    hashlineEffective: config.edit_mode === "hashline",
    storageDir: "/tmp/aft-test",
  };
}

/** Mock SDK ToolContext for test execute calls. */
function createMockSdkContext(directory: string): ToolContext {
  return {
    sessionID: "test",
    messageID: "test",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

type MetadataInput = Parameters<ToolContext["metadata"]>[0];

function createRecordingSdkContext(directory: string, metadataCalls: MetadataInput[]): ToolContext {
  return {
    ...createMockSdkContext(directory),
    metadata: (input) => {
      metadataCalls.push(input);
    },
  };
}

function recordingAsk(calls: Array<Record<string, unknown>>): ToolContext["ask"] {
  return (async (input: Record<string, unknown>) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}

function previewResponse(
  diff = "Index: preview.ts\n--- preview.ts\n+++ preview.ts\n",
  paths: { abs?: string[]; rel?: string[]; filepath?: string } = {},
): BridgeResponse {
  return {
    success: true,
    preview: true,
    preview_diff: diff,
    affected_paths: paths.abs ?? [],
    affected_rel_paths: paths.rel ?? [],
    filepath: paths.filepath ?? paths.rel?.[0] ?? "preview.ts",
    text: "Preview ready.",
  };
}

function isPreviewCall(call: SendCall | undefined): boolean {
  return call?.options?.preview === true;
}

function recordedToolCallOptions(options?: ToolCallOptions): ToolCallOptions | undefined {
  return options?.preview === true ? { preview: true } : undefined;
}

function createMockHoistedHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
    options?: ToolCallOptions,
  ) => Promise<BridgeResponse> | BridgeResponse,
  config: PluginContext["config"] = {} as PluginContext["config"],
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push({ command, params });
      return await sendImpl(command, params);
    },
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
      options?: ToolCallOptions,
    ) => {
      const recordedOptions = recordedToolCallOptions(options);
      calls.push({
        command: name,
        params: rawArgs,
        ...(recordedOptions ? { options: recordedOptions } : {}),
      });
      return await sendImpl(name, rawArgs, options);
    },
  };

  const pool = {
    getBridge: () => bridge,
  } as unknown as BridgePool;

  return {
    calls,
    tools: hoistedTools(createPluginContext(pool, config)),
  };
}

afterEach(async () => {
  if (tmpDir) {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = null;
  }
  sdkCtx = createMockSdkContext(PROJECT_CWD);
});

describe("Hoisted tool execute handlers", () => {
  test("read mirrors path-only input and updates OpenCode metadata", async () => {
    tmpDir = await makeTempDir();
    const metadataCalls: MetadataInput[] = [];
    const args: Record<string, unknown> = { path: "read.txt" };
    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return { success: true, text: "1: content\\n" };
    });

    await tools.read.execute(args, createRecordingSdkContext(tmpDir, metadataCalls));

    expect(args).toHaveProperty("filePath", "read.txt");
    expect(metadataCalls).toEqual([{ metadata: {} }]);
  });

  test("read preserves an existing filePath and skips invalid path aliasing", async () => {
    tmpDir = await makeTempDir();
    const metadataCalls: MetadataInput[] = [];
    const existing = { path: "read.txt", filePath: "read.txt" };
    const { tools } = createMockHoistedHarness(async () => ({ success: true, text: "ok" }));

    await tools.read.execute(existing, createRecordingSdkContext(tmpDir, metadataCalls));
    expect(existing.filePath).toBe("read.txt");

    const invalid: Record<string, unknown> = { path: "" };
    await expect(
      tools.read.execute(invalid, createRecordingSdkContext(tmpDir, metadataCalls)),
    ).rejects.toThrow();
    expect(invalid).not.toHaveProperty("filePath");
    expect(metadataCalls).toHaveLength(1);
  });

  test("write mirrors path-only input and updates OpenCode metadata", async () => {
    tmpDir = await makeTempDir();
    const metadataCalls: MetadataInput[] = [];
    const args: Record<string, unknown> = { path: "write.txt", content: "content\\n" };
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview === true
        ? { success: true, preview: true, preview_diff: "" }
        : { success: true, text: "Created new file." },
    );

    await tools.write.execute(args, createRecordingSdkContext(tmpDir, metadataCalls));

    expect(args).toHaveProperty("filePath", "write.txt");
    expect(metadataCalls).toEqual([{ metadata: {} }]);
  });

  test("write preserves an existing filePath and skips invalid path aliasing", async () => {
    tmpDir = await makeTempDir();
    const metadataCalls: MetadataInput[] = [];
    const existing = { path: "write.txt", filePath: "write.txt", content: "content\\n" };
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview === true
        ? { success: true, preview: true, preview_diff: "" }
        : { success: true, text: "Created new file." },
    );

    await tools.write.execute(existing, createRecordingSdkContext(tmpDir, metadataCalls));
    expect(existing.filePath).toBe("write.txt");

    const invalid: Record<string, unknown> = { path: "", content: "content\\n" };
    await expect(
      tools.write.execute(invalid, createRecordingSdkContext(tmpDir, metadataCalls)),
    ).rejects.toThrow();
    expect(invalid).not.toHaveProperty("filePath");
    expect(metadataCalls).toHaveLength(1);
  });

  test("edit mirrors path-only input and updates OpenCode metadata", async () => {
    tmpDir = await makeTempDir();
    const metadataCalls: MetadataInput[] = [];
    const args: Record<string, unknown> = { path: "edit.txt", appendContent: "content\\n" };
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview === true
        ? { success: true, preview: true, preview_diff: "" }
        : { success: true, text: "Edited (+1/-0)." },
    );

    await tools.edit.execute(args, createRecordingSdkContext(tmpDir, metadataCalls));

    expect(args).toHaveProperty("filePath", "edit.txt");
    expect(metadataCalls).toEqual([{ metadata: {} }]);
  });

  test("edit preserves an existing filePath and skips invalid path aliasing", async () => {
    tmpDir = await makeTempDir();
    const metadataCalls: MetadataInput[] = [];
    const existing = { path: "edit.txt", filePath: "edit.txt", appendContent: "content\\n" };
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview === true
        ? { success: true, preview: true, preview_diff: "" }
        : { success: true, text: "Edited (+1/-0)." },
    );

    await tools.edit.execute(existing, createRecordingSdkContext(tmpDir, metadataCalls));
    expect(existing.filePath).toBe("edit.txt");

    const invalid: Record<string, unknown> = { path: "", appendContent: "content\\n" };
    await expect(
      tools.edit.execute(invalid, createRecordingSdkContext(tmpDir, metadataCalls)),
    ).rejects.toThrow();
    expect(invalid).not.toHaveProperty("filePath");
    expect(metadataCalls).toHaveLength(1);
  });

  test("read throws the Rust error response instead of accessing missing content", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return { success: false, message: "File not found: missing.ts" };
    });

    await expect(tools.read.execute({ filePath: "missing.ts" }, sdkCtx)).rejects.toThrow(
      "File not found: missing.ts",
    );
  });

  test("restricted read forwards external bash artifacts to Rust without an external-directory ask", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = {
      ...createMockSdkContext(tmpDir),
      ask: recordingAsk(askCalls),
    };
    const artifactPath = resolve(tmpDir, "../bash-task.stdout");
    const { calls, tools } = createMockHoistedHarness(
      () => ({ success: true, text: "1: full bash output\n" }),
      { restrict_to_project_root: true } as PluginContext["config"],
    );

    const result = await tools.read.execute({ filePath: artifactPath }, sdkCtx);

    expect(text(result)).toContain("full bash output");
    expect(askCalls.map((call) => call.permission)).toEqual(["read"]);
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({
      command: "read",
      params: { filePath: artifactPath },
    });
  });

  test("read maps image attachments to OpenCode data URLs and metadata", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return {
        success: true,
        text: "Read image attachment (image/png, 32×16, 1 KB).",
        attachments: [
          {
            kind: "image",
            mime: "image/png",
            data: "aW1hZ2U=",
            bytes: 1024,
            base64_bytes: 8,
            width: 32,
            height: 16,
            resized: false,
            animation: "none",
            orientation_applied: false,
          },
        ],
      };
    });

    const result = (await tools.read.execute({ filePath: "image.png" }, sdkCtx)) as {
      output: string;
      attachments?: Array<{ type: string; mime: string; url: string }>;
      metadata?: { isImage?: boolean; isPdf?: boolean };
    };

    expect(result.output).toContain("Read image");
    expect(result.attachments).toEqual([
      { type: "file", mime: "image/png", url: "data:image/png;base64,aW1hZ2U=" },
    ]);
    expect(result.metadata?.isImage).toBe(true);
    expect(result.metadata?.isPdf).toBe(false);
  });

  test("read maps PDF attachments to OpenCode data URLs and metadata", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return {
        success: true,
        text: "Read PDF attachment (128 bytes).",
        attachments: [
          {
            kind: "pdf",
            mime: "application/pdf",
            data: "JVBERi0=",
            bytes: 128,
            base64_bytes: 8,
          },
        ],
      };
    });

    const result = (await tools.read.execute({ filePath: "doc.pdf" }, sdkCtx)) as {
      attachments?: Array<{ type: string; mime: string; url: string }>;
      metadata?: { isImage?: boolean; isPdf?: boolean };
    };

    expect(result.attachments).toEqual([
      { type: "file", mime: "application/pdf", url: "data:application/pdf;base64,JVBERi0=" },
    ]);
    expect(result.metadata?.isImage).toBe(false);
    expect(result.metadata?.isPdf).toBe(true);
  });

  test("write throws the Rust error response for invalid writes", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("write");
      return { success: false, message: "Refusing to write outside project root" };
    });

    await expect(
      tools.write.execute({ filePath: "../outside.ts", content: "export const x = 1;\n" }, sdkCtx),
    ).rejects.toThrow("Refusing to write outside project root");
  });

  test("write defaults diagnostics off and omits LSP payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview ? previewResponse() : { success: true, text: "File updated." },
    );

    const result = text(
      await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx),
    );

    expect(result).toBe("File updated.");
    expect(result).not.toContain("lsp_diagnostics");
    expect(result).not.toContain("LSP errors detected");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({
      command: "write",
      params: {
        filePath: "src/app.ts",
        content: "export {};\n",
      },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({
      command: "write",
      params: { filePath: "src/app.ts", content: "export {};\n" },
    });
  });

  test("write leaves diagnostics server-owned and out of agent arguments", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview ? previewResponse() : { success: true, text: "File updated." },
    );

    await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx);

    await tools.write.execute(
      { filePath: "src/app.ts", content: "export {};\n", diagnostics: false },
      sdkCtx,
    );
    expect(calls).toHaveLength(4);
    for (const call of calls) {
      expect(call.params.diagnostics).toBeUndefined();
      expect(call.params.include_diff_content).toBeUndefined();
      expect(call.params.preview).toBeUndefined();
    }
  });

  test("write surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : {
            success: true,
            text: "File updated.\n\nLSP errors detected, please fix:\n  Line 7: Bad type\n",
          },
    );

    const result = text(
      await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx),
    );

    expect(calls.every((call) => call.params.diagnostics === undefined)).toBe(true);
    expect(result).toContain("LSP errors detected");
    expect(result).toContain("Line 7: Bad type");
  });

  test("edit defaults diagnostics off and omits LSP payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 1, text: "Edited (+0/-0)." },
    );

    const result = text(
      await tools.edit.execute(
        { filePath: "src/app.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    );

    // Agent-facing result is the compact summary sentence, not raw JSON.
    expect(result).toBe("Edited (+0/-0).");
    expect(result).not.toContain("lsp_diagnostics");
    expect(result).not.toContain("LSP errors detected");
    expect(result).not.toContain("backup_id");
    const previewCall = calls.find(isPreviewCall);
    const realCall = calls.find((call) => !isPreviewCall(call));
    expect(previewCall?.command).toBe("edit");
    expect(previewCall?.params.diagnostics).toBeUndefined();
    expect(previewCall?.params.include_diff_content).toBeUndefined();
    expect(realCall?.command).toBe("edit");
    expect(realCall?.params.diagnostics).toBeUndefined();
    expect(realCall?.params.include_diff_content).toBeUndefined();
  });

  test("edit surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const diagnostics = [{ severity: "error", line: 3, message: "Missing import" }];
    const { calls, tools } = createMockHoistedHarness(
      async (_command, _params, options) =>
        options?.preview
          ? previewResponse()
          : {
              success: true,
              replacements: 1,
              lsp_diagnostics: diagnostics,
              text: "Edited (+0/-0).\n\nLSP errors detected, please fix:\n  Line 3: Missing import",
            },
      { lsp: { diagnostics_on_edit: true } } as PluginContext["config"],
    );

    const result = text(
      await tools.edit.execute(
        { filePath: "src/app.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    );

    // Headline is the compact summary; LSP errors are appended below it.
    expect(result.split("\n\n")[0]).toBe("Edited (+0/-0).");
    const realCall = calls.find((call) => !isPreviewCall(call));
    expect(realCall?.params.diagnostics).toBeUndefined();
    expect(result).toContain("Line 3: Missing import");
  });

  test("write and edit return patch-based filediff metadata for normal responses", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const before = "export const value = 1;\n";
    const after = "export const value = 2;\n";

    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : {
            success: true,
            created: false,
            replacements: 1,
            diff: { before, after, additions: 1, deletions: 1 },
            text: "Mutation complete.",
          },
    );

    const writeResult = (await tools.write.execute(
      { filePath: "write.ts", content: after },
      sdkCtx,
    )) as { metadata?: Record<string, unknown> };
    const editResult = (await tools.edit.execute(
      { filePath: "edit.ts", oldString: "value = 1", newString: "value = 2" },
      sdkCtx,
    )) as { metadata?: Record<string, unknown> };

    for (const result of [writeResult, editResult]) {
      const metadata = result.metadata as
        | {
            diff?: string;
            filediff?: {
              file?: string;
              patch?: string;
              additions?: number;
              deletions?: number;
            };
          }
        | undefined;
      expect(metadata?.filediff).toMatchObject({ additions: 1, deletions: 1 });
      expect(metadata?.filediff?.file).toBeTypeOf("string");
      expect(metadata?.filediff?.patch).toContain("-export const value = 1;");
      expect(metadata?.filediff?.patch).toContain("+export const value = 2;");
      expect(metadata?.diff).toBe(metadata?.filediff?.patch);
      expect(metadata?.filediff).not.toHaveProperty("before");
      expect(metadata?.filediff).not.toHaveProperty("after");
    }
  });

  test("write and edit return the preview patch for truncated responses", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const previewDiff = `Index: large.ts
--- large.ts
+++ large.ts
@@ -1,1 +1,1 @@
-old line
+new line
`;

    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse(previewDiff)
        : {
            success: true,
            created: false,
            replacements: 1,
            diff: { additions: 1, deletions: 1, truncated: true },
            text: "Mutation complete.",
          },
    );

    const writeResult = (await tools.write.execute(
      { filePath: "large-write.ts", content: "new line\n" },
      sdkCtx,
    )) as { metadata?: Record<string, unknown> };
    const editResult = (await tools.edit.execute(
      { filePath: "large-edit.ts", oldString: "old line", newString: "new line" },
      sdkCtx,
    )) as { metadata?: Record<string, unknown> };

    for (const result of [writeResult, editResult]) {
      const metadata = result.metadata as
        | {
            diff?: string;
            filediff?: {
              file?: string;
              patch?: string;
              additions?: number;
              deletions?: number;
            };
          }
        | undefined;
      expect(metadata?.filediff?.file).toBeTypeOf("string");
      expect(metadata?.diff).toBe(previewDiff);
      expect(metadata?.filediff).toMatchObject({
        patch: previewDiff,
        additions: 1,
        deletions: 1,
      });
      expect(metadata?.filediff?.patch).toContain("-old line");
      expect(metadata?.filediff?.patch).toContain("+new line");
      expect(metadata?.filediff).not.toHaveProperty("before");
      expect(metadata?.filediff).not.toHaveProperty("after");
    }
  });

  test("omits filediff when a truncated response has no preview patch", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? { success: true, text: "Preview ready." }
        : {
            success: true,
            diff: { additions: 1, deletions: 1, truncated: true },
            text: "Mutation complete.",
          },
    );

    const result = (await tools.write.execute(
      { filePath: "large-write.ts", content: "new line\n" },
      sdkCtx,
    )) as { metadata?: { diff?: string; filediff?: unknown } };

    expect(result.metadata?.diff).toBe("");
    expect(result.metadata?.filediff).toBeUndefined();
  });

  test("apply_patch calls server preview then apply without diagnostics payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const patchText = `*** Begin Patch
*** Add File: file.ts
+export const value = 1;
*** End Patch`;
    const previewDiff = `Index: file.ts
--- file.ts
+++ file.ts
@@ -0,0 +1,1 @@
+export const value = 1;
`;

    const { calls, tools } = createMockHoistedHarness(async (command, params, options) => {
      expect(command).toBe("apply_patch");
      expect(params).toEqual({ patchText });
      expect(params.diagnostics).toBeUndefined();
      if (options?.preview) {
        return previewResponse(previewDiff, {
          abs: [resolve(tmpDir as string, "file.ts")],
          rel: ["file.ts"],
          filepath: "file.ts",
        });
      }
      return {
        success: true,
        text: "Created file.ts",
        output: "Created file.ts",
        metadata: { diff: previewDiff, files: [] },
      };
    });

    const result = text(await tools.apply_patch.execute({ patchText }, sdkCtx));

    expect(result).toBe("Created file.ts");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toEqual({
      command: "apply_patch",
      params: { patchText },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({ command: "apply_patch", params: { patchText } });
  });

  test("mutation tool schemas expose no per-call diagnostics param", () => {
    const { tools } = createMockHoistedHarness(async () => ({ success: true }));
    const pool = {
      getBridge: () => ({ send: async () => ({ success: true }) }),
    } as unknown as BridgePool;
    const prefixedTools = hoistedTools(createPluginContext(pool));

    // Removed deliberately: agents never used it; diagnostics are the status
    // bar (passive) + aft_inspect (pull) + the lsp.diagnostics_on_edit config.
    for (const toolDef of [
      tools.write,
      tools.edit,
      tools.apply_patch,
      prefixedTools.write,
      prefixedTools.edit,
      prefixedTools.apply_patch,
    ]) {
      expect(toolDef.args.diagnostics).toBeUndefined();
      expect(toolDef.args.preview).toBeUndefined();
      expect(toolDef.args.dryRun).toBeUndefined();
    }
  });

  test("write approval ask includes unified diff metadata for existing changes", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    await writeFile(resolve(tmpDir, "src.ts"), "export const value = 1;\n");

    const previewDiff =
      "Index: src.ts\n--- src.ts\n+++ src.ts\n@@ -1,1 +1,1 @@\n-export const value = 1;\n+export const value = 2;\n";
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse(previewDiff)
        : { success: true, created: false, text: "File updated." },
    );

    await tools.write.execute({ filePath: "src.ts", content: "export const value = 2;\n" }, sdkCtx);

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "src.ts"));
    expect(editAsk?.metadata?.diff).toBeTypeOf("string");
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("write approval ask includes all-additions diff metadata for a new file", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const previewDiff =
      "Index: new.ts\n--- new.ts\n+++ new.ts\n@@ -0,0 +1,1 @@\n+export const fresh = true;\n";
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse(previewDiff)
        : { success: true, created: true, text: "Created new file." },
    );

    await tools.write.execute(
      { filePath: "new.ts", content: "export const fresh = true;\n" },
      sdkCtx,
    );

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "new.ts"));
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toBeTypeOf("string");
    expect(diff).toBe(previewDiff);
  });

  test("edit oldString approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const previewDiff =
      "Index: src.ts\n--- src.ts\n+++ src.ts\n@@ -1,1 +1,1 @@\n-const value = 1;\n+const value = 2;\n";
    const { tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) {
        return {
          success: true,
          preview_diff: previewDiff,
          text: "Preview ready.",
        };
      }
      return { success: true, replacements: 1, text: "Edited (+1/-1)." };
    });

    await tools.edit.execute({ filePath: "src.ts", oldString: "1", newString: "2" }, sdkCtx);

    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("hashline edit runs preflight, preview permission, then apply", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const patch = "*** Begin Patch\n[src.ts#ABCD]\nPUT 1:\n+after\n*** End Patch";
    const previewDiff = "Index: src.ts\n--- src.ts\n+++ src.ts\n";

    const { calls, tools } = createMockHoistedHarness(
      async (command, _params, options) => {
        if (command === "hashline_preflight") {
          return {
            success: true,
            affected_paths: [resolve(tmpDir as string, "src.ts")],
            affected_rel_paths: ["src.ts"],
            mv_destinations: [],
            text: "Preflight ready.",
          };
        }
        expect(command).toBe("edit");
        if (options?.preview) {
          return {
            success: true,
            preview: true,
            text: "Preview ready.",
            metadata: { diff: previewDiff },
          };
        }
        return {
          success: true,
          filePath: "src.ts",
          text: "1 of 1 files applied",
          metadata: { diff: previewDiff, files: [] },
        };
      },
      { edit_mode: "hashline" },
    );

    const result = await tools.edit.execute({ patch }, sdkCtx);

    expect(calls.map((call) => [call.command, call.options?.preview === true])).toEqual([
      ["hashline_preflight", false],
      ["edit", true],
      ["edit", false],
    ]);
    expect(askCalls.find((call) => call.permission === "edit")?.patterns).toContain("src.ts");
    expect(askCalls.find((call) => call.permission === "edit")?.metadata?.surface).toBe("hashline");
    expect(result.output).toBe("1 of 1 files applied");
  });

  test("edit symbol approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const previewDiff =
      "Index: symbol.ts\n--- symbol.ts\n+++ symbol.ts\n@@ -1,1 +1,1 @@\n-export function oldName() {}\n+export function newName() {}\n";

    const { calls, tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("edit");
      if (options?.preview) return previewResponse(previewDiff);
      return {
        success: true,
        symbol: "oldName",
        operation: "replace",
        diff: { additions: 1, deletions: 1 },
        text: "Edited (+1/-1).",
      };
    });

    await tools.edit.execute(
      { filePath: "symbol.ts", symbol: "oldName", content: "export function newName() {}\n" },
      sdkCtx,
    );

    expect(calls[0]?.options?.preview).toBe(true);
    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("edit batch approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const previewDiff =
      "Index: batch.ts\n--- batch.ts\n+++ batch.ts\n@@ -1,2 +1,2 @@\n-before\n+after\n";

    const { tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("edit");
      if (options?.preview) return previewResponse(previewDiff);
      return { success: true, edits_applied: 1, text: "Edited (+1/-1)." };
    });

    await tools.edit.execute(
      { filePath: "batch.ts", edits: [{ oldString: "before", newString: "after" }] },
      sdkCtx,
    );

    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("edit preview errors surface before asking for approval", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) return { success: false, message: "match_not_found", text: "" };
      throw new Error("real edit should not run after preview failure");
    });

    await expect(
      tools.edit.execute(
        { filePath: "missing.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    ).rejects.toThrow("match_not_found");
    expect(askCalls).toHaveLength(0);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.options?.preview).toBe(true);
  });

  test("apply_patch approval ask uses the Rust preview diff and affected paths", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const patchText = `*** Begin Patch
*** Add File: new.ts
+added line
*** End Patch`;
    const previewDiff = `Index: new.ts
--- new.ts
+++ new.ts
@@ -0,0 +1,1 @@
+added line
`;

    const { calls, tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("apply_patch");
      if (options?.preview) {
        return previewResponse(previewDiff, {
          abs: [resolve(tmpDir as string, "new.ts")],
          rel: ["new.ts"],
          filepath: "new.ts",
        });
      }
      return {
        success: true,
        text: "Created new.ts",
        metadata: { diff: previewDiff, files: [] },
      };
    });

    await tools.apply_patch.execute({ patchText }, sdkCtx);

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.patterns).toEqual(["new.ts"]);
    expect(editAsk?.metadata?.filepath).toBe("new.ts");
    expect(editAsk?.metadata?.diff).toBe(previewDiff);
    expect(calls.map((call) => call.command)).toEqual(["apply_patch", "apply_patch"]);
    expect(calls[0]?.options).toEqual({ preview: true });
  });

  test("apply_patch preview errors surface before asking for approval", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const patchText = `*** Begin Patch
*** Update File: file.ts
@@
-expected
+new
*** End Patch`;

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) => {
      expect(options?.preview).toBe(true);
      return { success: false, message: "Failed to update file.ts", text: "" };
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Failed to update file.ts",
    );
    expect(askCalls).toHaveLength(0);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.options?.preview).toBe(true);
  });

  test("edit preserves logical code, exact message, and structured cause for failed replacements", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const message =
      "batch: edits[0] match 'same' is ambiguous (2 occurrences, expected 1). Use 'occurrence' (1-based) to select one, or 'replaceAll': true to replace every occurrence.";
    const response = {
      success: false,
      code: "ambiguous_match",
      message,
      occurrences: [
        { index: 1, line: 1, context: "same same" },
        { index: 2, line: 1, context: "same same" },
      ],
      text: `wrapped: ${message}`,
    };
    const { tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("edit");
      if (options?.preview) return response;
      throw new Error("real edit should not run after preview failure");
    });

    try {
      await tools.edit.execute(
        { filePath: "target.ts", oldString: "before", newString: "after" },
        sdkCtx,
      );
      throw new Error("expected edit failure");
    } catch (error) {
      expect((error as { code?: string }).code).toBe("ambiguous_match");
      expect((error as Error).message).toBe(message);
      expect(
        (error as { cause?: { code?: string; message?: string; response?: unknown } }).cause,
      ).toMatchObject({
        code: "ambiguous_match",
        message,
        response,
      });
    }
  });

  // Regression: Rust reverts a write that fails syntax validation and returns
  // success:true with rolled_back:true. Reporting "File updated." then would be
  // a lie — the file is unchanged. The agent must be told it rolled back.
  test("write reports a rolled-back write honestly, not as success", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("write");
      return options?.preview
        ? previewResponse()
        : {
            success: true,
            rolled_back: true,
            created: false,
            text: "Write rolled back: the content produced invalid syntax, so the file was left unchanged.",
          };
    });

    const result = text(
      await tools.write.execute({ filePath: "target.ts", content: "const = ;\n" }, sdkCtx),
    );
    expect(result.toLowerCase()).toContain("rolled back");
    expect(result).not.toContain("File updated");
  });

  test("apply_patch throws when the server reports total failure", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const patchText = `*** Begin Patch
*** Add File: broken.ts
+export const broken = true;
*** End Patch`;

    const { tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) {
        return previewResponse(
          `Index: broken.ts
--- broken.ts
+++ broken.ts
`,
          {
            abs: [resolve(tmpDir as string, "broken.ts")],
            rel: ["broken.ts"],
            filepath: "broken.ts",
          },
        );
      }
      return {
        success: false,
        code: "apply_patch_failed",
        message: "Patch failed — none of the 1 hunk(s) applied: broken.ts.",
        text: "Patch failed — none of the 1 hunk(s) applied: broken.ts.",
        output: "Patch failed — none of the 1 hunk(s) applied: broken.ts.",
      };
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Patch failed — none of the 1 hunk(s) applied: broken.ts.",
    );
  });

  test("delete throws when every file in the batch fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("delete");
      const files = (params.files as string[]) ?? [];
      return {
        success: true,
        text: `Deleted 0/${files.length} file(s)`,
        complete: false,
        deleted: [],
        skipped_files: files.map((file) => ({ file, reason: "Cannot delete protected file" })),
      };
    });

    await expect(
      tools.aft_delete.execute({ files: ["locked.ts", "also-locked.ts"] }, sdkCtx),
    ).rejects.toThrow("Cannot delete protected file");
  });

  test("delete throws the Rust error response before synthesizing success", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("delete");
      return { success: false, message: "bridge delete refused" };
    });

    await expect(tools.aft_delete.execute({ files: ["doomed.ts"] }, sdkCtx)).rejects.toThrow(
      "bridge delete refused",
    );
  });

  test("delete returns readable partial-success text when some files fail", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("delete");
      const files = (params.files as string[]) ?? [];
      const deleted: Array<{ file: string; backup_id: string | null }> = [];
      const skipped: Array<{ file: string; reason: string }> = [];
      for (const file of files) {
        if (file.includes("blocked.ts")) {
          skipped.push({ file, reason: "permission denied" });
        } else {
          deleted.push({ file, backup_id: null });
        }
      }
      return {
        success: true,
        text: `Deleted ${deleted.length}/${files.length} file(s)`,
        complete: skipped.length === 0,
        deleted,
        skipped_files: skipped,
      };
    });

    const result = await tools.aft_delete.execute(
      { files: ["a.ts", "blocked.ts", "c.ts"] },
      sdkCtx,
    );
    expect(result).toBe("Deleted 2/3 file(s)");
    expect(result.trim().startsWith("{")).toBe(false);
  });

  test("delete returns readable text when every file succeeds", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("delete");
      const files = (params.files as string[]) ?? [];
      return {
        success: true,
        text: `Deleted ${files.length}/${files.length} file(s)`,
        complete: true,
        deleted: files.map((file) => ({ file, backup_id: null })),
        skipped_files: [],
      };
    });

    const result = await tools.aft_delete.execute({ files: ["a.ts", "b.ts"] }, sdkCtx);
    expect(result).toBe("Deleted 2/2 file(s)");
    expect(result.trim().startsWith("{")).toBe(false);
  });

  test("move throws the Rust error response when rename fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("move");
      return { success: false, message: "Destination already exists" };
    });

    await expect(
      tools.aft_move.execute({ filePath: "source.ts", destination: "dest.ts" }, sdkCtx),
    ).rejects.toThrow("Destination already exists");
  });

  test("edit batch schema accepts replaceAll and coerces stringified item values", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, edits_applied: 2, text: "Edited (+0/-0, 2 edits)." },
    );
    const editsSchema = tools.edit.args.edits as unknown as {
      safeParse: (value: unknown) => { success: boolean };
    };
    expect(
      editsSchema.safeParse([{ oldString: "before", newString: "after", replaceAll: true }])
        .success,
    ).toBe(true);

    const result = text(
      await tools.edit.execute(
        {
          filePath: "batch.ts",
          edits: [
            {
              oldString: "before",
              newString: "after",
              replaceAll: "true" as unknown as boolean,
            },
            { startLine: 4, endLine: 6, content: "replacement" },
          ],
        },
        sdkCtx,
      ),
    );

    // 2 edits applied, no diff in the mock -> counts default to 0.
    expect(result).toBe("Edited (+0/-0, 2 edits).");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({
      command: "edit",
      params: {
        path: "batch.ts",
        edits: [
          { oldString: "before", newString: "after", replaceAll: true },
          { startLine: 4, endLine: 6, content: "replacement" },
        ],
      },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({
      command: "edit",
      params: {
        path: "batch.ts",
        edits: [
          { oldString: "before", newString: "after", replaceAll: true },
          { startLine: 4, endLine: 6, content: "replacement" },
        ],
      },
    });
  });

  test('legacy edit mode:"write" form is rejected before reaching the bridge', async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    // The retired mode/file form must never reach the bridge: the boundary
    // rejects it with steering toward the canonical surface.
    const pool = {
      getBridge: () => ({
        send: async () => {
          throw new Error("bridge must not be called for the retired form");
        },
      }),
    } as unknown as BridgePool;
    const tools = hoistedTools(createPluginContext(pool));

    await expect(
      tools.edit.execute(
        { mode: "write", file: "legacy.ts", content: "export const x = 1;\n" },
        sdkCtx,
      ),
    ).rejects.toThrow(/retired `mode`\/`file` edit form|Unrecognized keys/);
  });

  test("edit forwards replaceAll to Rust for multiple occurrences", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 3, text: "Edited (+0/-0, 3 replacements)." },
    );

    const result = text(
      await tools.edit.execute(
        {
          filePath: "repeated.ts",
          oldString: "oldName",
          newString: "newName",
          replaceAll: true,
        },
        sdkCtx,
      ),
    );

    // replaceAll with 3 replacements -> count surfaced; no diff -> +0/-0.
    // The top-level single-edit form folds into a one-item edits[] at the
    // boundary before the wire call.
    expect(result).toBe("Edited (+0/-0, 3 replacements).");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({
      command: "edit",
      params: {
        path: "repeated.ts",
        edits: [{ oldString: "oldName", newString: "newName", replaceAll: true }],
      },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({
      command: "edit",
      params: {
        path: "repeated.ts",
        edits: [{ oldString: "oldName", newString: "newName", replaceAll: true }],
      },
    });
  });

  test('edit forwards string replaceAll "true" to Rust replace_all', async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 1, text: "Edited (+0/-0)." },
    );

    await tools.edit.execute(
      {
        filePath: "repeated.ts",
        oldString: "oldName",
        newString: "newName",
        replaceAll: "true" as unknown as boolean,
      },
      sdkCtx,
    );

    const applyCall = calls.find((c) => c.command === "edit" && !isPreviewCall(c));
    const foldedEdits = applyCall?.params.edits as Array<Record<string, unknown>>;
    expect(foldedEdits).toHaveLength(1);
    expect(foldedEdits[0].replaceAll).toBe(true);
  });

  test('edit rejects occurrence "0" and coerces string occurrence "1" (1-based)', async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 1, text: "Edited (+0/-0)." },
    );

    // occurrence is 1-based: 0 is rejected at the boundary before any wire call.
    await expect(
      tools.edit.execute(
        {
          filePath: "repeated.ts",
          oldString: "oldName",
          newString: "newName",
          occurrence: "0" as unknown as number,
        },
        sdkCtx,
      ),
    ).rejects.toThrow(/positive integer/);
    expect(calls.filter((c) => c.command === "edit")).toHaveLength(0);

    // A stringified "1" coerces to the first occurrence and folds into edits[].
    await tools.edit.execute(
      {
        filePath: "repeated.ts",
        oldString: "oldName",
        newString: "newName",
        occurrence: "1" as unknown as number,
      },
      sdkCtx,
    );

    const applyCall = calls.find((c) => c.command === "edit" && !isPreviewCall(c));
    const foldedEdits = applyCall?.params.edits as Array<Record<string, unknown>>;
    expect(foldedEdits[0].occurrence).toBe(1);
  });

  /// Diff-payload contract: the server returns full before/after for UI
  /// metadata, but the agent-facing result must stay as compact rendered text.
  /// Echoing before/after into the model context makes the payload scale with
  /// file size, not edit size.
  test("edit agent result strips diff before/after to counts-only", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const bigBefore = `${"x".repeat(50_000)}\n`;
    const bigAfter = `${"y".repeat(50_000)}\n`;
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : {
            success: true,
            replacements: 1,
            text: "Edited (+1/-1).",
            diff: { before: bigBefore, after: bigAfter, additions: 1, deletions: 1 },
          },
    );

    const result = text(
      await tools.edit.execute({ filePath: "big.ts", oldString: "x", newString: "y" }, sdkCtx),
    );

    // Agent result must NOT contain the 50KB file content from either side.
    expect(result).not.toContain(bigBefore);
    expect(result).not.toContain(bigAfter);
    expect(result.length).toBeLessThan(2_000);

    // Counts survive for the agent's verification signal, in the compact
    // summary sentence (no raw JSON, no before/after content).
    expect(result).toBe("Edited (+1/-1).");
  });

  test("apply_patch returns partial success text and metadata from the server", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const patchText = `*** Begin Patch
*** Add File: created.ts
+export const created = true;
*** Add File: broken.ts
+export const broken = true;
*** End Patch`;
    const diff = `Index: created.ts
--- created.ts
+++ created.ts
@@ -0,0 +1,1 @@
+export const created = true;
`;
    const files = [
      {
        filePath: resolve(tmpDir, "created.ts"),
        relativePath: "created.ts",
        type: "add",
        patch: diff,
        additions: 1,
        deletions: 0,
      },
    ];

    const { tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) {
        return previewResponse(diff, {
          abs: [resolve(tmpDir as string, "created.ts"), resolve(tmpDir as string, "broken.ts")],
          rel: ["created.ts", "broken.ts"],
          filepath: "created.ts",
        });
      }
      return {
        success: true,
        partial: true,
        text: `Created created.ts
Failed to create broken.ts: simulated failure
Patch partially applied — 1 of 2 hunk(s) succeeded. Failed: broken.ts.`,
        title: "Applied 1 of 2 hunks",
        metadata: { diff, files },
      };
    });

    const result = (await tools.apply_patch.execute({ patchText }, sdkCtx)) as {
      output: string;
      title?: string;
      metadata?: { diff?: string; files?: unknown[] };
    };

    expect(result.output).toContain("Patch partially applied");
    expect(result.title).toBe("Applied 1 of 2 hunks");
    expect(result.metadata?.diff).toBe(diff);
    expect(result.metadata?.files).toEqual(files);
  });

  test("read returns binary-file messages without trying to split missing content", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "Binary file (512 bytes)",
      binary: true,
      message: "Binary file (512 bytes)",
    }));

    const result = text(await tools.read.execute({ filePath: "artifact.bin" }, sdkCtx));

    expect(result).toBe("Binary file (512 bytes)");
    expect(calls[0]).toEqual({
      command: "read",
      params: {
        filePath: "artifact.bin",
      },
    });
  });

  test("read handles directory listings and truncated content responses", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    let callIndex = 0;
    const { tools } = createMockHoistedHarness(async () => {
      callIndex += 1;
      if (callIndex === 1) {
        return { success: true, text: "a.ts\nsrc/", entries: ["a.ts", "src/"] };
      }

      return {
        success: true,
        text: "1: one\n2: two\n(Showing lines 1-2 of 10. Use startLine/endLine to read other sections.)",
        content: "1: one\n2: two",
        truncated: true,
        start_line: 1,
        end_line: 2,
        total_lines: 10,
      };
    });

    const directoryResult = text(await tools.read.execute({ filePath: "." }, sdkCtx));
    const truncatedResult = text(await tools.read.execute({ filePath: "big.ts" }, sdkCtx));

    expect(directoryResult).toBe("a.ts\nsrc/");
    // Case B: agent did NOT specify a range, response was clamped → hint footer
    // is useful, tells the agent more exists and how to get it.
    expect(truncatedResult).toBe(
      "1: one\n2: two\n(Showing lines 1-2 of 10. Use startLine/endLine to read other sections.)",
    );
  });

  test("read does not append a footer when the file fits in default limit (case A)", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "1: one\n2: two\n3: three",
      content: "1: one\n2: two\n3: three",
      // truncated:false means the response IS the whole file — no footer needed.
      truncated: false,
      start_line: 1,
      end_line: 3,
      total_lines: 3,
    }));

    const result = text(await tools.read.execute({ filePath: "small.ts" }, sdkCtx));

    expect(result).toBe("1: one\n2: two\n3: three");
  });

  test("read drops the navigation hint when the agent supplied startLine/endLine (case B)", async () => {
    // Repro for the dogfooding bug: agent calls read({startLine: 130, endLine: 190})
    // on a 191-line file and gets back lines 130-190 EXACTLY. Telling them
    // "use startLine/endLine to read other sections" right after they used
    // those exact params is patronizing. They have the math.
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "130: ...\n190: ...",
      content: "130: ...\n190: ...",
      // Rust sets truncated:true for any file slice (end_line < total_lines).
      // The server must not base its truncation hint only on that flag; it must
      // also check whether the agent explicitly chose the slice via startLine/endLine.
      truncated: true,
      start_line: 130,
      end_line: 190,
      total_lines: 191,
    }));

    const result = text(
      await tools.read.execute({ filePath: "registry.ts", startLine: 130, endLine: 190 }, sdkCtx),
    );

    // The user's exact complaint: when end_line matches total_lines (or is
    // close to it after a deliberate range), no footer should be emitted at
    // all. Agent gets back only the content.
    expect(result).toBe("130: ...\n190: ...");
    expect(result).not.toContain("Use startLine/endLine");
  });

  test("read drops the footer entirely when the agent's range happens not to cover the full file (case B)", async () => {
    // Subtle case: agent asked 100-150 of a 200-line file. They got back
    // exactly what they asked for. The earlier "compact when clamped"
    // branch would have spuriously emitted `(Lines 100-150 of 200)` here,
    // which is the SAME shape of patronizing footer as the original bug —
    // re-teaching an agent that they got less than the whole file when
    // THEY chose to. Agent has the math: they sent the request and they
    // can see the content length.
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "100: ...\n150: ...",
      content: "100: ...\n150: ...",
      truncated: true,
      start_line: 100,
      end_line: 150,
      total_lines: 200,
    }));

    const result = text(
      await tools.read.execute({ filePath: "mid.ts", startLine: 100, endLine: 150 }, sdkCtx),
    );

    expect(result).toBe("100: ...\n150: ...");
    expect(result).not.toContain("Use startLine/endLine");
    expect(result).not.toContain("(Lines 100-150");
  });

  test("read drops the navigation hint when the agent supplied offset/limit (case B)", async () => {
    // Same as the startLine/endLine case but for the OpenCode-built-in-
    // compatible offset/limit param shape. Agent that picked the slice
    // should not be re-taught how to pick a slice.
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "10: ...\n29: ...",
      content: "10: ...\n29: ...",
      truncated: true,
      start_line: 10,
      end_line: 29,
      total_lines: 30,
    }));

    const result = text(
      await tools.read.execute({ filePath: "small.ts", offset: 10, limit: 20 }, sdkCtx),
    );

    // No footer at all — agent picked the range, has the math.
    expect(result).toBe("10: ...\n29: ...");
    expect(result).not.toContain("Use startLine/endLine");
    expect(result).not.toContain("(Lines");
    expect(calls[0]).toEqual({
      command: "read",
      params: { filePath: "small.ts", startLine: 10, endLine: 29 },
    });
  });

  test("write distinguishes new files from updates", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    let writeCount = 0;
    const { calls, tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("write");
      if (options?.preview) return previewResponse();
      writeCount += 1;
      return writeCount === 1
        ? { success: true, created: true, formatted: false, text: "Created new file." }
        : { success: true, created: false, formatted: true, text: "File updated. Auto-formatted." };
    });

    const createdResult = text(
      await tools.write.execute(
        { filePath: "created.ts", content: "export const created = true;\n" },
        sdkCtx,
      ),
    );
    const updatedResult = text(
      await tools.write.execute(
        { filePath: "created.ts", content: "export const created = false;\n" },
        sdkCtx,
      ),
    );

    expect(createdResult).toBe("Created new file.");
    expect(updatedResult).toBe("File updated. Auto-formatted.");
    expect(calls).toHaveLength(4);
    expect(calls[0]?.params.filePath).toBe("created.ts");
    expect(calls[1]?.params.filePath).toBe("created.ts");
    expect(calls[2]?.params.filePath).toBe("created.ts");
    expect(calls[3]?.params.filePath).toBe("created.ts");
  });

  test("foreground bash abort calls bash_abort_inflight", async () => {
    tmpDir = await makeTempDir();
    const controller = new AbortController();
    controller.abort();
    const calls: SendCall[] = [];
    const bridge = {
      send: async (command: string, params: Record<string, unknown> = {}) => {
        calls.push({ command, params });
        if (command === "bash_abort_inflight") return { success: true, killed: 0 };
        return { success: true, status: "completed", output: "ok", task_id: "bash-test" };
      },
    };
    const pool = { getBridge: () => bridge } as unknown as BridgePool;
    const tools = hoistedTools(createPluginContext(pool));
    const runtime = { ...createMockSdkContext(tmpDir), abort: controller.signal } as ToolContext;

    await tools.bash.execute({ command: "sleep 1", wait: true }, runtime);

    expect(calls.map((call) => call.command)).toContain("bash_abort_inflight");
  });

  test("foreground bash completion removes the abort listener", async () => {
    tmpDir = await makeTempDir();
    const controller = new AbortController();
    const calls: string[] = [];
    const bridge = {
      send: async (command: string) => {
        calls.push(command);
        return { success: true, status: "completed", output: "ok", task_id: "bash-test" };
      },
    };
    const pool = { getBridge: () => bridge } as unknown as BridgePool;
    const tools = hoistedTools(createPluginContext(pool));
    const runtime = { ...createMockSdkContext(tmpDir), abort: controller.signal } as ToolContext;

    await tools.bash.execute({ command: "echo ok", wait: true }, runtime);
    controller.abort();
    await Promise.resolve();

    expect(calls).toEqual(["bash"]);
  });

  test("apply_patch passes server per-file diff metadata to the OpenCode renderer", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    const patchText = `*** Begin Patch
*** Add File: new.ts
+export const created = 1;
*** End Patch`;
    const diff = `Index: new.ts
--- new.ts
+++ new.ts
@@ -0,0 +1,1 @@
+export const created = 1;
`;
    const files = [
      {
        filePath: resolve(tmpDir, "new.ts"),
        relativePath: "new.ts",
        type: "add",
        patch: diff,
        additions: 1,
        deletions: 0,
      },
    ];

    const { tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) {
        return previewResponse(diff, {
          abs: [resolve(tmpDir as string, "new.ts")],
          rel: ["new.ts"],
          filepath: "new.ts",
        });
      }
      return {
        success: true,
        text: "Created new.ts",
        title: "Applied 1 hunks",
        metadata: { diff, files },
      };
    });

    const stored = (await tools.apply_patch.execute({ patchText }, sdkCtx)) as {
      output: string;
      title?: string;
      metadata?: { diff?: string; files?: typeof files };
    };

    expect(stored.output).toBe("Created new.ts");
    expect(stored.title).toBe("Applied 1 hunks");
    expect(stored.metadata?.diff).toBe(diff);
    expect(stored.metadata?.files).toEqual(files);
  });
});

/**
 * Registration of `bash` and its companions never depends on the bash runtime
 * configuration: `bash: false`, `bash.enabled: false` or `bash.background:
 * false` change runtime behavior (the engine answers `bash_disabled`), while
 * only `disabled_tools` removes a registration.
 */
describe("Hoisted bash registration is independent of runtime bash config", () => {
  function toolsWithConfig(cfg: Partial<PluginContext["config"]>): Record<string, unknown> {
    const pool = { getBridge: () => ({ send: async () => ({}) }) } as unknown as BridgePool;
    const ctx: PluginContext = {
      pool,
      client: createMockClient(),
      config: cfg as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    return hoistedTools(ctx);
  }

  for (const [label, cfg] of [
    ["no bash config", {}],
    ["bash: true", { bash: true }],
    ["bash: false", { bash: false }],
    ["bash.enabled: false", { bash: { enabled: false } }],
    ["bash.background: false", { bash: { background: false } }],
    ["legacy rewrite only", { experimental: { bash: { rewrite: true } } }],
  ] as const) {
    test(`${label} → bash and every companion registered`, () => {
      const tools = toolsWithConfig(cfg as Partial<PluginContext["config"]>);
      for (const name of ["bash", "bash_status", "bash_write", "bash_watch", "bash_kill"]) {
        expect(tools[name], name).toBeDefined();
      }
      expect(tools.read).toBeDefined();
      expect(tools.edit).toBeDefined();
    });
  }
});
