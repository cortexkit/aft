/**
 * Unit tests for hoisted read/write/edit/grep argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import { formatReadFooter, registerHoistedTools } from "../tools/hoisted.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

const roots: string[] = [];

async function tempRoot(): Promise<string> {
  const root = join(tmpdir(), `aft-pi-hoisted-${process.pid}-${roots.length}-${Date.now()}`);
  roots.push(root);
  await mkdir(root, { recursive: true });
  return root;
}

function toolArgs(call: { params: Record<string, unknown> }): Record<string, unknown> {
  return call.params.arguments as Record<string, unknown>;
}

function schemaAccepts(schema: unknown, value: unknown): boolean {
  return Value.Check(schema as TSchema, value);
}

function expectRootObjectSchema(schema: unknown): void {
  const record = schema as Record<string, unknown>;
  expect(record.type).toBe("object");
  expect(record.anyOf).toBeUndefined();
  expect(record.oneOf).toBeUndefined();
  expect(record.allOf).toBeUndefined();
}

function schemaHasProperty(schema: unknown, property: string): boolean {
  const record = schema as { properties?: Record<string, unknown> };
  return record.properties?.[property] !== undefined;
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("hoisted tool adapters", () => {
  test("read/write/edit schemas stay plain root object schemas", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: true,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    expectRootObjectSchema(tools.get("read")!.parameters);
    expectRootObjectSchema(tools.get("write")!.parameters);
    const editSchema = tools.get("edit")!.parameters as {
      properties?: {
        edits?: { items?: { properties?: Record<string, Record<string, unknown>> } };
      };
    };
    expectRootObjectSchema(editSchema);
    expect(schemaHasProperty(editSchema, "edits")).toBe(true);
    expect(
      schemaAccepts(editSchema, {
        path: "batch.ts",
        edits: [
          { oldString: "before", newString: "after", replaceAll: true },
          { startLine: 2, endLine: 3, content: "replacement" },
        ],
      }),
    ).toBe(true);
    expect(
      schemaAccepts(editSchema, {
        path: "batch.ts",
        edits: [{ startLine: "2", endLine: "3", content: "replacement" }],
      }),
    ).toBe(true);

    const batchReplaceAllSchema = editSchema.properties?.edits?.items?.properties?.replaceAll;
    expect(batchReplaceAllSchema?.type).toBe("boolean");
    expect(batchReplaceAllSchema?.description).toContain("every occurrence");

    const batchStartLineSchema = editSchema.properties?.edits?.items?.properties?.startLine;
    expect(batchStartLineSchema?.description).toBe(
      "1-based start line for a batch line-range edit",
    );
    expect(Array.isArray(batchStartLineSchema?.anyOf)).toBe(true);
    const batchStartLineVariants = (batchStartLineSchema?.anyOf ?? []) as Array<
      Record<string, unknown>
    >;
    expect(
      batchStartLineVariants.some(
        (variant) =>
          variant.type === "integer" &&
          variant.minimum === 1 &&
          variant.maximum === Number.MAX_SAFE_INTEGER,
      ),
    ).toBe(true);
    expect(batchStartLineVariants.some((variant) => variant.type === "string")).toBe(true);
  });

  test("registers prefixed alternatives while keeping bare Rust command names", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistBuiltinTools: false,
      hoistRead: true,
      hoistWrite: true,
      hoistEdit: true,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    expect([...tools.keys()].sort()).toEqual(["aft_edit", "aft_grep", "aft_read", "aft_write"]);
    expect(tools.has("read")).toBe(false);
    expect(tools.has("write")).toBe(false);
    expect(tools.has("edit")).toBe(false);
    expect(tools.has("grep")).toBe(false);

    await executeTool(tools.get("aft_edit")!, {
      path: "target.ts",
      edits: [{ oldString: "before", newString: "after" }],
    });

    // The registration alias is local to Pi; AFT's bridge API stays bare.
    expect(calls[0].params.name).toBe("edit");
  });

  test("hashline edit pins one session across preflight, preview, and apply", async () => {
    const root = await tempRoot();
    let editCalls = 0;
    let sessionReads = 0;
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) => {
      if (command === "hashline_preflight") {
        return {
          success: true,
          affected_paths: [join(root, "src.ts")],
          affected_rel_paths: ["src.ts"],
          mv_destinations: [],
          text: "Preflight ready.",
        };
      }
      expect(command).toBe("edit");
      editCalls += 1;
      return editCalls === 1
        ? { success: true, preview: true, text: "Preview ready." }
        : {
            success: true,
            filePath: "src.ts",
            text: "1 of 1 files applied",
            metadata: { diff: "Index: src.ts\n", files: [] },
          };
    });
    registerHoistedTools(
      api,
      makePluginContext(bridge, {
        config: { edit_mode: "hashline" },
        hashlineEffective: true,
      }),
      {
        hoistRead: false,
        hoistWrite: false,
        hoistEdit: true,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    const editSchema = tools.get("edit")!.parameters as {
      properties?: Record<string, unknown>;
      required?: string[];
    };
    expect(Object.keys(editSchema.properties ?? {})).toEqual(["patch"]);
    expect(editSchema.required).toEqual(["patch"]);
    const description = tools.get("edit")!.description;
    for (const fragment of [
      "[path#TAG]",
      "current tagged read",
      "`N.=M`",
      "plain `N` PUT replaces",
      "`PUT <address>:`",
      "`+` alone is blank",
      "REM: bare `REM` only",
      "MV: `MV <destination>`",
    ]) {
      expect(description).toContain(fragment);
    }
    const extCtx = makeExtContext(root, "hashline-session");
    const mutableSession = extCtx as unknown as {
      sessionManager: { getSessionId: () => string };
    };
    mutableSession.sessionManager.getSessionId = () => {
      sessionReads += 1;
      return sessionReads === 1 ? "hashline-session" : "concurrent-session";
    };
    const result = (await executeTool(
      tools.get("edit")!,
      { patch: "*** Begin Patch\n[src.ts#ABCD]\nPUT 1:\n+after\n*** End Patch" },
      extCtx,
    )) as { content: Array<{ text: string }> };

    expect(calls.map((call) => [call.params.name, call.params.preview === true])).toEqual([
      ["hashline_preflight", false],
      ["edit", true],
      ["edit", false],
    ]);
    expect(calls.map((call) => call.params.session_id)).toEqual([
      "hashline-session",
      "hashline-session",
      "hashline-session",
    ]);
    expect(sessionReads).toBe(1);
    expect(result.content[0].text).toContain("1 of 1 files applied");
  });

  test("read maps offset/limit to inclusive start_line/end_line and appends footer", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      text:
        params.offset === undefined
          ? "1: a\n2: b\n(Showing lines 1-2 of 10. Use offset/limit to read other sections.)"
          : "1: a\n2: b",
      truncated: true,
      start_line: 1,
      end_line: 2,
      total_lines: 10,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const ranged = (await executeTool(tools.get("read")!, {
      path: "src/app.ts",
      offset: 5,
      limit: 3,
    })) as { content: Array<{ text: string }> };

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("read");
    expect(toolArgs(calls[0])).toEqual({ filePath: "src/app.ts", offset: 5, limit: 3 });
    expect(ranged.content[0].text).not.toContain("Use offset/limit");

    const unbounded = (await executeTool(tools.get("read")!, { path: "src/app.ts" })) as {
      content: Array<{ text: string }>;
    };
    expect(unbounded.content[0].text).toContain("Showing lines 1-2 of 10");
  });

  test("restricted read forwards external bash artifacts to Rust", async () => {
    const root = await tempRoot();
    const artifactPath = join(root, "..", "bash-task.stdout");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "1: full bash output\n",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: artifactPath }, {
      ...makeExtContext(),
      cwd: root,
    } as never)) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toContain("full bash output");
    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toEqual({ filePath: artifactPath });
  });

  test("read accepts filePath as a compatibility alias and rejects unequal dual spelling", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });
    const readTool = tools.get("read")!;

    // The raw Pi schema publishes only the canonical path spelling. The
    // prepareArguments hook still accepts filePath before execution.
    expect(schemaAccepts(readTool.parameters, { filePath: "alias.ts" })).toBe(false);
    await executeTool(readTool, { filePath: "alias.ts" });
    expect(toolArgs(calls[0])).toEqual({ filePath: "alias.ts" });

    expect(schemaAccepts(readTool.parameters, { path: "canonical.ts" })).toBe(true);
    expect(schemaHasProperty(readTool.parameters, "filePath")).toBe(false);
    await expect(
      executeTool(readTool, { path: "canonical.ts", filePath: "alias.ts" }),
    ).rejects.toThrow(/equal decoded strings/);

    expect(schemaAccepts(readTool.parameters, {})).toBe(false);
    await expect(executeTool(readTool, {})).rejects.toThrow("'path' is required");
  });

  test("read emits image content for vision-capable Pi models", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
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
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: "image.png" }, {
      ...makeExtContext(),
      model: { input: ["text", "image"] },
    } as never)) as {
      content: Array<{ type: string; text?: string; data?: string; mimeType?: string }>;
    };

    expect(result.content[0]).toMatchObject({ type: "text" });
    expect(result.content[1]).toEqual({ type: "image", data: "aW1hZ2U=", mimeType: "image/png" });
  });

  test("read omits image content for non-vision Pi models", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
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
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: "image.png" }, {
      ...makeExtContext(),
      model: { input: ["text"] },
    } as never)) as { content: Array<{ type: string; text?: string }> };

    expect(result.content).toHaveLength(1);
    expect(result.content[0].type).toBe("text");
    expect(result.content[0].text).toContain("Current model does not support images");
  });

  test("read reports PDFs as unsupported text on Pi", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      attachments: [
        {
          kind: "pdf",
          mime: "application/pdf",
          data: "JVBERi0=",
          bytes: 128,
          base64_bytes: 8,
        },
      ],
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: "doc.pdf" })) as {
      content: Array<{ type: string; text?: string }>;
    };

    expect(result.content).toHaveLength(1);
    expect(result.content[0].type).toBe("text");
    expect(result.content[0].text).toContain("PDFs aren't supported on the Pi harness yet.");
  });

  test("edit appendContent forwards raw agent args through tool_call preview and mutate", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    await executeTool(tools.get("edit")!, {
      path: "README.md",
      appendContent: "\nnext",
    });

    expect(calls.map((call) => call.command)).toEqual(["tool_call"]);
    expect(calls[0].params).toMatchObject({ name: "edit" });
    expect(toolArgs(calls[0])).toEqual({
      path: "README.md",
      appendContent: "\nnext",
    });
  });

  test("edit preserves logical code, exact message, and structured cause for failed replacements", async () => {
    const { api, tools } = makeMockApi();
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
    const { bridge } = makeMockBridge(() => response);
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    try {
      await executeTool(tools.get("edit")!, {
        path: "same.txt",
        edits: [{ oldString: "same", newString: "new" }],
      });
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

  test("edit batch mode coerces stringified item replaceAll through tool_call", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "Edited (+2/-2, 2 edits).",
      edits_applied: 2,
      diff: { additions: 2, deletions: 2 },
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("edit")!, {
      path: "batch.ts",
      edits: [
        {
          oldString: "before",
          newString: "after",
          replaceAll: "true" as unknown as boolean,
        },
        { startLine: 4, endLine: 6, content: "replacement" },
      ],
    })) as { content: Array<{ text: string }>; details: { editsApplied?: number } };

    expect(result.content[0].text).toBe("Edited (+2/-2, 2 edits).");
    expect(result.details.editsApplied).toBe(2);
    expect(calls.map((call) => call.command)).toEqual(["tool_call"]);
    expect(calls[0].params).toMatchObject({ name: "edit" });
    expect(toolArgs(calls[0])).toEqual({
      path: "batch.ts",
      edits: [
        { oldString: "before", newString: "after", replaceAll: true },
        { startLine: 4, endLine: 6, content: "replacement" },
      ],
    });
  });

  test("edit rejects requests that contain more than one mode", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "unexpected mutation",
      diff: { additions: 1, deletions: 1 },
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    await expect(
      executeTool(tools.get("edit")!, {
        path: "modes.ts",
        appendContent: "\nnext",
        edits: [{ startLine: 2, endLine: 2, content: "ignored" }],
      }),
    ).rejects.toThrow(/conflicting modes/);
    expect(calls).toHaveLength(0);
  });

  test("edit rejects legacy find/replace fields combined with an explicit mode", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    await expect(
      executeTool(tools.get("edit")!, {
        path: "modes.ts",
        edits: [{ oldString: "before", newString: "after" }],
        oldString: "before",
        newString: "after",
      }),
    ).rejects.toThrow(/conflicting modes/);
    expect(calls).toHaveLength(0);
  });

  test("edit rejects invalid batch edit shapes before calling the bridge", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "unused" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    await expect(
      executeTool(tools.get("edit")!, {
        path: "broken.ts",
        edits: [{ startLine: 2, content: "replacement" }],
      }),
    ).rejects.toThrow("edit: edits[0].endLine must be a positive integer");
    expect(calls).toHaveLength(0);
  });

  test("edit accepts filePath as a compatibility alias without overriding path", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });
    const editTool = tools.get("edit")!;

    // The raw schema is edits[]-only; top-level oldString/newString remain a
    // compatibility input that is folded before the tool call is dispatched.
    expect(
      schemaAccepts(editTool.parameters, {
        path: "canonical.ts",
        edits: [{ oldString: "before", newString: "after" }],
      }),
    ).toBe(true);
    expect(
      schemaAccepts(editTool.parameters, {
        filePath: "alias.ts",
        oldString: "before",
        newString: "after",
      }),
    ).toBe(false);
    await executeTool(editTool, {
      filePath: "alias.ts",
      oldString: "before",
      newString: "after",
    });
    expect(toolArgs(calls[0])).toEqual({
      path: "alias.ts",
      edits: [{ oldString: "before", newString: "after" }],
    });

    expect(schemaHasProperty(editTool.parameters, "filePath")).toBe(false);
    await expect(
      executeTool(editTool, {
        path: "canonical.ts",
        filePath: "alias.ts",
        oldString: "before",
        newString: "after",
      }),
    ).rejects.toThrow(/equal decoded strings/);

    expect(schemaAccepts(editTool.parameters, { oldString: "before", newString: "after" })).toBe(
      false,
    );
    await expect(
      executeTool(editTool, { oldString: "before", newString: "after" }),
    ).rejects.toThrow("'path' is required");
  });

  test("edit defaults diagnostics off and omits LSP payload", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      replacements: 1,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("edit")!, {
      path: "src/app.ts",
      oldString: "before",
      newString: "after",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls.map((call) => call.command)).toEqual(["tool_call"]);
    expect(calls[0].params).toMatchObject({ name: "edit" });
    expect(toolArgs(calls[0])).toMatchObject({
      path: "src/app.ts",
      edits: [{ oldString: "before", newString: "after" }],
    });
    expect(toolArgs(calls[0])).not.toHaveProperty("diagnostics");
    expect(result.content[0].text).not.toContain("LSP diagnostics");
    expect(result.details.diagnostics).toBeUndefined();
  });

  test("edit surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    const diagnostics = [{ severity: "error", line: 5, message: "Broken edit" }];
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      replacements: 1,
      lsp_diagnostics: diagnostics,
    }));
    registerHoistedTools(
      api,
      makePluginContext(bridge, { config: { lsp: { diagnostics_on_edit: true } } }),
      {
        hoistRead: false,
        hoistWrite: false,
        hoistEdit: true,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    const result = (await executeTool(tools.get("edit")!, {
      path: "src/app.ts",
      oldString: "before",
      newString: "after",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(toolArgs(calls[0])).not.toHaveProperty("diagnostics");
    expect(result.details.diagnostics).toEqual(diagnostics);
    expect(result.content[0].text).toContain("LSP diagnostics");
    expect(result.content[0].text).toContain("Broken edit");
  });

  test("grep resolves existing path args and preserves brace-aware include globs", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    await writeFile(join(root, "src", "app.ts"), "console.log('x');\n");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: "src", include: "*.ts,**/*.{tsx,jsx}" },
      { cwd: root } as never,
    );

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("grep");
    // Rust grep does not consume context_lines, so Pi no longer advertises or
    // forwards it (parity with OpenCode grep, which never exposed it).
    expect(toolArgs(calls[0])).toEqual({
      pattern: "console",
      path: join(root, "src"),
      include: "*.ts,**/*.{tsx,jsx}",
    });
  });

  test("grep expands ~ in path arg to the user's home directory", async () => {
    // Agents commonly type `~/Work/...` paths. Without expansion, Node's
    // path.resolve treats `~` as a literal directory, the existence check
    // fails, and Rust receives the unresolved path. Expansion must happen
    // before stat() so absolute tilde paths resolve like the shell would.
    const home = (await import("node:os")).homedir();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    await executeTool(tools.get("grep")!, { pattern: "oauth", path: "~/" }, { cwd: home } as never);

    expect(calls[0].command).toBe("tool_call");
    // When the expanded path equals the home directory itself, stat()
    // succeeds and resolvePathArg returns the absolute form.
    expect(toolArgs(calls[0])).toEqual({ pattern: "oauth", path: home });
  });

  test("grep searches existing fragments and reports skipped missing paths", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    const missing = join(root, "test");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      complete: true,
      text: "src/app.ts:1: console.log('x');",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: `${join(root, "src")} ${missing}` },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }>; details: { complete?: boolean } };

    expect(calls[0].command).toBe("tool_call");
    expect(toolArgs(calls[0]).path).toBe(join(root, "src"));
    expect(result.content[0].text).toContain("src/app.ts:1");
    expect(result.content[0].text).toContain(`Skipped 1 path not found: ${missing}`);
    expect(result.details.complete).toBe(false);
  });

  test("grep keeps all-valid multi-path searches complete", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    await mkdir(join(root, "e2e"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      complete: true,
      text: "ok",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: `${join(root, "src")} ${join(root, "e2e")}` },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }>; details: { complete?: boolean } };

    expect(calls[0].command).toBe("tool_call");
    expect(toolArgs(calls[0]).path).toBe(`${join(root, "src")} ${join(root, "e2e")}`);
    expect(result.content[0].text).toBe("ok");
    expect(result.content[0].text).not.toContain("Skipped");
    expect(result.details.complete).toBe(true);
  });

  test("grep falls through to path_not_found when every fragment is missing", async () => {
    const root = await tempRoot();
    const missingA = join(root, "missing-a");
    const missingB = join(root, "missing-b");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: false,
      code: "path_not_found",
      message: "grep: search path does not exist",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    let thrown: unknown;
    try {
      await executeTool(
        tools.get("grep")!,
        { pattern: "console", path: `${missingA} ${missingB}` },
        { cwd: root } as never,
      );
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    expect((thrown as Error).message).toContain("grep: search path does not exist");
    expect(toolArgs(calls[0]).path).toBe(`${missingA} ${missingB}`);
  });

  test("grep treats an existing single path containing a space as one path", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "with space"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      complete: true,
      text: "ok",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: "with space" },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }>; details: { complete?: boolean } };

    expect(calls[0].command).toBe("tool_call");
    expect(toolArgs(calls[0]).path).toBe(join(root, "with space"));
    expect(result.content[0].text).toBe("ok");
    expect(result.content[0].text).not.toContain("Skipped");
    expect(result.details.complete).toBe(true);
  });

  test("write defaults diagnostics off and asks Rust for a diff", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls.map((call) => call.command)).toEqual(["tool_call"]);
    expect(toolArgs(calls[0])).toEqual({
      filePath: "src/app.ts",
      content: "export {};\n",
    });
    expect(result.content[0].text).not.toContain("LSP diagnostics");
    expect(result.details.diagnostics).toBeUndefined();
  });

  test("write accepts filePath as a compatibility alias and rejects unequal dual spelling", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });
    const writeTool = tools.get("write")!;

    expect(schemaAccepts(writeTool.parameters, { path: "canonical.ts", content: "x" })).toBe(true);
    expect(schemaAccepts(writeTool.parameters, { filePath: "alias.ts", content: "x" })).toBe(false);
    await executeTool(writeTool, { filePath: "alias.ts", content: "x" });
    expect(toolArgs(calls[0])).toEqual({ filePath: "alias.ts", content: "x" });

    expect(schemaHasProperty(writeTool.parameters, "filePath")).toBe(false);
    await expect(
      executeTool(writeTool, { path: "canonical.ts", filePath: "alias.ts", content: "x" }),
    ).rejects.toThrow(/equal decoded strings/);

    expect(schemaAccepts(writeTool.parameters, { content: "x" })).toBe(false);
    await expect(executeTool(writeTool, { content: "x" })).rejects.toThrow("'path' is required");
  });

  test("write follows lsp.diagnostics_on_edit (config-driven; no per-call param)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(
      api,
      makePluginContext(bridge, { config: { lsp: { diagnostics_on_edit: true } } }),
      {
        hoistRead: false,
        hoistWrite: true,
        hoistEdit: false,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    });
    expect(toolArgs(calls[0])).not.toHaveProperty("diagnostics");

    // The per-call `diagnostics` param was removed (agents never used it; the
    // status bar + aft_inspect are the agent-facing diagnostics paths). A
    // stray param must NOT override the configured default.
    await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
      diagnostics: false,
    });
    expect(toolArgs(calls[1])).not.toHaveProperty("diagnostics");
  });

  test("write surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    const diagnostics = [{ severity: "error", line: 11, message: "Broken write" }];
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      lsp_diagnostics: diagnostics,
    }));
    registerHoistedTools(
      api,
      makePluginContext(bridge, { config: { lsp: { diagnostics_on_edit: true } } }),
      {
        hoistRead: false,
        hoistWrite: true,
        hoistEdit: false,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    const result = (await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls.map((call) => call.command)).toEqual(["tool_call"]);
    expect(toolArgs(calls[0])).not.toHaveProperty("diagnostics");
    expect(result.details.diagnostics).toEqual(diagnostics);
    expect(result.content[0].text).toContain("LSP diagnostics");
    expect(result.content[0].text).toContain("Broken write");
  });

  test("mutation schemas expose no per-call diagnostics param", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // Removed deliberately: agents never used it; diagnostics are the status
    // bar (passive) + aft_inspect (pull) + the lsp.diagnostics_on_edit config.
    expect(schemaHasProperty(tools.get("write")!.parameters, "diagnostics")).toBe(false);
    expect(schemaHasProperty(tools.get("edit")!.parameters, "diagnostics")).toBe(false);
  });

  test("restrict=true hard-blocks external write (no prompt) before the bridge", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // restrict_to_project_root is AFT's full-isolation knob — NOT a per-call
    // prompt. An external path is hard-blocked with a clear, actionable error
    // (Pi's tool-result surface) and never reaches the bridge. No ui.confirm.
    const externalPath = join(tmpdir(), `aft-external-${process.pid}-${Date.now()}.txt`);
    const extCtx = { cwd: root, hasUI: false };

    await expect(
      executeTool(tools.get("write")!, { filePath: externalPath, content: "x" }, extCtx as never),
    ).rejects.toThrow(/restrict_to_project_root/);
    expect(calls).toEqual([]);
  });

  test("external path proceeds to the bridge when restrictToProjectRoot is false", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      // User opted in to "no restriction" — Pi has no host-level allow-list
      // to consult, so AFT defers to Rust (which accepts the path).
      restrictToProjectRoot: false,
    });

    const externalPath = join(tmpdir(), `aft-external-norestrict-${process.pid}-${Date.now()}.txt`);
    const extCtx = { cwd: root, hasUI: false };

    await executeTool(
      tools.get("write")!,
      { filePath: externalPath, content: "x" },
      extCtx as never,
    );

    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toMatchObject({ filePath: externalPath, content: "x" });
  });

  test("formatReadFooter only hints when Rust clamped an unbounded read", () => {
    expect(
      formatReadFooter(false, { truncated: true, start_line: 1, end_line: 100, total_lines: 500 }),
    ).toBe("\n(Showing lines 1-100 of 500. Use offset/limit to read other sections.)");
    expect(
      formatReadFooter(true, { truncated: true, start_line: 1, end_line: 100, total_lines: 500 }),
    ).toBe("");
    expect(formatReadFooter(false, { truncated: true })).toBe("");
  });
});

describe("file: URL path arguments", () => {
  test("read resolves a file:/// URL to the local path", async () => {
    // resolvePathArg is the single choke point every hoisted Pi tool routes
    // path arguments through; a file: URL must reach the bridge as a plain
    // absolute path, byte-identical to what the Rust decoder would produce.
    const { resolvePathArg } = await import("../tools/hoisted.js");
    const dir = await mkdtemp(join(tmpdir(), "aft-pi-fileurl-"));
    try {
      const target = join(dir, "a b.txt");
      await writeFile(target, "x");
      const url = `file://${target.replaceAll(" ", "%20")}`;
      expect(await resolvePathArg(dir, url)).toBe(target);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("malformed escapes decode tolerantly (Rust parity)", async () => {
    const { resolvePathArg } = await import("../tools/hoisted.js");
    // %2e%2e decodes to .. even with a trailing malformed %ZZ — matching the
    // byte-wise Rust decoder, NOT decodeURIComponent's all-or-nothing throw.
    const resolved = await resolvePathArg("/proj", "file:///proj/%2e%2e/%ZZ/x");
    expect(resolved).toBe("/proj/../%ZZ/x");
  });
});

describe("GitHub resource mutation permissions", () => {
  test("write shows the exact body before publishing and denial sends nothing", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "published" }));
    registerHoistedTools(api, makePluginContext(bridge, { config: { github: { write: true } } }), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: false,
    });
    const confirmations: Array<{ title: string; body: string }> = [];
    const extCtx = {
      ...makeExtContext(),
      hasUI: true,
      ui: {
        confirm: async (title: string, body: string) => {
          confirmations.push({ title, body });
          return false;
        },
      },
      signal: new AbortController().signal,
    } as ReturnType<typeof makeExtContext>;
    const body = "Pi-visible GitHub body\nsecond line";

    await expect(
      executeTool(tools.get("write")!, { path: "issue://7", content: body }, extCtx),
    ).rejects.toThrow("was not published");
    expect(confirmations).toEqual([{ title: "Publish GitHub comment?", body }]);
    expect(calls).toEqual([]);
  });
});

describe("conditional GitHub mutation descriptions", () => {
  test("write and edit advertise resource forms only when github.write is effective", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerHoistedTools(api, makePluginContext(bridge, { config: { github: { write: true } } }), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: false,
    });
    expect(tools.get("write")!.description).toContain('write("issue://N", content)');
    expect(tools.get("edit")!.description).toContain('edit("issue://N/comments/K"');
  });
});
