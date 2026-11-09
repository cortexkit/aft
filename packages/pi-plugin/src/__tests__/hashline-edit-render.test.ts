/**
 * The hashline `edit` row in Pi: the call names the files its patch addresses
 * and the result shows the diff Rust returns in `metadata.files`.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentToolResult } from "@earendil-works/pi-coding-agent";
import type { Component } from "@earendil-works/pi-tui";
import { buildHashlineMutationResult, registerHoistedTools } from "../tools/hoisted.js";
import { makeContext, mockTheme, renderToString } from "./render-test-helpers.js";
import {
  executeTool,
  type MockToolDef,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

interface DisplayFile {
  path: string;
  type: "update" | "add" | "delete" | "move";
  hunks: string;
  additions: number;
  deletions: number;
  sourcePath?: string;
}

/** `crates/aft/src/edit.rs::build_unified_diff`: an `Index:` banner, file headers, then hunks. */
function unifiedPatch(path: string, hunks: string): string {
  return `Index: ${path}\n${"=".repeat(67)}\n--- ${path}\n+++ ${path}\n${hunks}`;
}

/**
 * A successful hashline `edit` reply as the Pi transport sends it
 * (`crates/aft/src/hashline/integration/transport.rs::render_mutation_response`),
 * plus the `text` the tool_call layer copies from `output`. The absolute-path
 * fields of the real reply are left out: the Pi adapter does not read them.
 */
function hashlineReply(files: DisplayFile[]): Record<string, unknown> {
  const metaFiles = files.map((file) => {
    const patch = unifiedPatch(file.path, file.hunks);
    return {
      relativePath: file.path,
      type: file.type,
      patch,
      additions: file.additions,
      deletions: file.deletions,
      ...(file.sourcePath ? { movePath: file.sourcePath, sourcePath: file.sourcePath } : {}),
    };
  });
  const output = [
    `${files.length} of ${files.length} files applied`,
    ...files.map((file) => `primary ${file.path}: applied (applied)\n[${file.path}#1A2B]`),
  ].join("\n");
  return {
    output,
    text: output,
    title: "Hashline edit applied",
    success: true,
    complete: true,
    partial: false,
    all_failed: false,
    preview: false,
    metadata: {
      diff: metaFiles.map((file) => file.patch).join("\n"),
      files: metaFiles,
    },
    op_id: "op-1",
    hashline: true,
    registers_committed: false,
    transport: "pi",
  };
}

const EDIT_A: DisplayFile = {
  path: "src/a.ts",
  type: "update",
  hunks: "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n",
  additions: 1,
  deletions: 1,
};

const EDIT_B: DisplayFile = {
  path: "src/b.ts",
  type: "update",
  hunks: "@@ -1 +1,2 @@\n x\n+y\n",
  additions: 1,
  deletions: 0,
};

const MOVE: DisplayFile = {
  path: "new.ts",
  type: "move",
  hunks: "@@ -0,0 +1 @@\n+keep\n",
  additions: 1,
  deletions: 0,
  sourcePath: "old.ts",
};

const roots: string[] = [];

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

/** Register the hashline `edit` tool against a bridge whose `edit` reply is `reply`. */
async function hashlineEditTool(reply: Record<string, unknown> = {}) {
  const root = join(tmpdir(), `aft-pi-hashline-render-${process.pid}-${roots.length}`);
  roots.push(root);
  await mkdir(root, { recursive: true });
  const { api, tools } = makeMockApi();
  const { bridge } = makeMockBridge((command) =>
    command === "hashline_preflight"
      ? { success: true, affected_paths: [], mv_destinations: [] }
      : reply,
  );
  registerHoistedTools(
    api,
    makePluginContext(bridge, { config: { edit_mode: "hashline" }, hashlineEffective: true }),
    {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    },
  );
  const tool = tools.get("edit") as MockToolDef;
  return { tool, extCtx: makeExtContext(root, "hashline-render") };
}

function renderCall(tool: MockToolDef, patch: unknown): string {
  expect(tool.renderCall).toBeDefined();
  const args = { patch };
  return renderToString(tool.renderCall?.(args, mockTheme, makeContext(args)) as Component);
}

function renderResult(tool: MockToolDef, result: unknown, expanded: boolean): string {
  expect(tool.renderResult).toBeDefined();
  return renderToString(
    tool.renderResult?.(
      result,
      { expanded, isPartial: false },
      mockTheme,
      makeContext({ patch: "unused" }),
    ) as Component,
  );
}

describe("hashline edit call row", () => {
  test("names the file of a single-section patch", async () => {
    const { tool } = await hashlineEditTool();
    expect(renderCall(tool, "[src/a.ts#1A2B]\nPUT 2:\n+TWO\n")).toBe("edit src/a.ts");
  });

  test("names the first file and counts the others in a multi-file patch", async () => {
    const { tool } = await hashlineEditTool();
    const patch = [
      "*** Begin Patch",
      "[src/a.ts#1A2B]",
      "PUT 2:",
      "+TWO",
      "[src/b.ts#3C4D]",
      "PUT >1:",
      "+y",
      "[src/a.ts#1A2B]",
      "CUT 3",
      "[src/c.ts#5E6F]",
      "REM",
      "*** End Patch",
    ].join("\n");
    // src/a.ts appears in two sections but is one file.
    expect(renderCall(tool, patch)).toBe("edit src/a.ts and 2 more");
  });

  test("names the source and destination of a move", async () => {
    const { tool } = await hashlineEditTool();
    expect(renderCall(tool, "[old.ts#1A2B]\nMV new.ts\n")).toBe("edit old.ts → new.ts");
  });

  test("keeps the file while the model is still writing the patch", async () => {
    const { tool } = await hashlineEditTool();
    // The last line is cut mid-directive, as Pi shows it while arguments stream.
    expect(renderCall(tool, "[src/a.ts#1A2B]\nPU")).toBe("edit src/a.ts");
    expect(renderCall(tool, "[src/a.ts#1A")).toBe("edit ...");
    expect(renderCall(tool, undefined)).toBe("edit ...");
  });
});

describe("hashline edit result", () => {
  test("maps a single-file reply into the diff details the edit row renders", () => {
    const reply = hashlineReply([EDIT_A]);
    const result = buildHashlineMutationResult(reply);

    expect(result.content).toEqual([{ type: "text", text: reply.text as string }]);
    expect(result.details?.diff).toBe(" 1 one\n-2 two\n+2 TWO\n 3 three");
    expect(result.details?.additions).toBe(1);
    expect(result.details?.deletions).toBe(1);
    expect(result.details?.firstChangedLine).toBe(2);
    expect(result.details?.files).toEqual([
      {
        path: "src/a.ts",
        sourcePath: undefined,
        diff: result.details?.diff,
        additions: 1,
        deletions: 1,
      },
    ]);
  });

  test("keeps every file of a multi-file reply in the diff and the rendered result", async () => {
    const reply = hashlineReply([EDIT_A, EDIT_B]);
    const { tool, extCtx } = await hashlineEditTool(reply);
    const result = (await executeTool(
      tool,
      { patch: "[src/a.ts#1A2B]\nPUT 2:\n+TWO\n[src/b.ts#3C4D]\nPUT >1:\n+y\n" },
      extCtx,
    )) as AgentToolResult<{
      diff?: string;
      additions: number;
      deletions: number;
      firstChangedLine?: number;
    }>;

    expect(result.content).toEqual([{ type: "text", text: reply.text as string }]);
    expect(result.details.diff).toBe(
      [
        "src/a.ts (+1/-1)",
        " 1 one",
        "-2 two",
        "+2 TWO",
        " 3 three",
        "src/b.ts (+1/-0)",
        " 1 x",
        "+2 y",
      ].join("\n"),
    );
    expect(result.details.additions).toBe(2);
    expect(result.details.deletions).toBe(1);
    expect(result.details.firstChangedLine).toBe(2);

    const expanded = renderResult(tool, result, true);
    for (const fragment of ["src/a.ts (+1/-1)", "+2 TWO", "src/b.ts (+1/-0)", "+2 y"]) {
      expect(expanded).toContain(fragment);
    }
    expect(renderResult(tool, result, false)).toBe("edited src/a.ts and 1 more (+2/-1)");
  });

  test("shows a moved file's diff and names its source", async () => {
    const reply = hashlineReply([MOVE]);
    const { tool } = await hashlineEditTool(reply);
    const result = buildHashlineMutationResult(reply);

    expect(result.details?.diff).toBe("+1 keep");
    expect(result.details?.additions).toBe(1);
    expect(result.details?.deletions).toBe(0);
    expect(result.details?.firstChangedLine).toBe(1);
    expect(result.details?.files?.[0]?.sourcePath).toBe("old.ts");
    expect(renderResult(tool, result, true)).toContain("+1 keep");
  });
});
