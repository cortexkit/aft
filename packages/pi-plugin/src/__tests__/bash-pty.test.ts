import { afterEach, describe, expect, test } from "bun:test";
import { appendFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerBashTool } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

interface MockToolDef {
  name: string;
  label: string;
  description: string;
  parameters: Record<string, unknown>;
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: ((update: unknown) => void) | undefined,
    ctx: { cwd: string },
  ) => Promise<unknown>;
}

const tempDirs: string[] = [];

async function addArtifactBytes(
  command: string,
  params: Record<string, unknown>,
  response: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  if (command !== "bash_status" || response.success === false) return response;
  const data = { ...response };
  const path = data.output_path;
  if (typeof path !== "string") return data;
  const bytes = await readFile(path);
  if (data.mode === "pty") {
    const offset = Math.max(0, Number(params.output_offset ?? 0));
    data.output_chunk_base64 = bytes.subarray(offset).toString("base64");
    data.output_next_offset = bytes.length;
    if (params.output_mode === "raw" || params.output_mode === "both") {
      data.pty_raw = bytes.toString("utf8");
    }
  }
  return data;
}

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

function api(tools: Map<string, MockToolDef>): ExtensionAPI {
  return {
    registerTool: (tool: MockToolDef) => tools.set(tool.name, tool),
  } as unknown as ExtensionAPI;
}

function ctx(
  send: (
    command: string,
    params: Record<string, unknown>,
  ) => Record<string, unknown> | Promise<Record<string, unknown>>,
) {
  const calls: Array<[string, Record<string, unknown>]> = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push([command, params]);
      return addArtifactBytes(command, params, await send(command, params));
    },
  } as unknown as BinaryBridge;
  return {
    calls,
    ctx: {
      pool: { getBridge: () => bridge } as PluginContext["pool"],
      config: {} as PluginContext["config"],
      storageDir: "/tmp/test",
    } satisfies PluginContext,
  };
}

async function spill(contents: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "aft-pi-pty-test-"));
  tempDirs.push(dir);
  const file = join(dir, "task.pty");
  await writeFile(file, contents);
  return file;
}

function text(result: unknown): string {
  return (result as { content: Array<{ type: string; text: string }> }).content[0].text;
}

describe("Pi bash PTY layer", () => {
  test("PTY incomplete capture status preserves command outcome and shows warning", async () => {
    const reason = "PTY output may be incomplete: output capture failed before EOF";
    for (const exitCode of [0, 17]) {
      for (const outputMode of ["screen", "raw", "both"]) {
        const status = exitCode === 0 ? "completed" : "failed";
        const tools = new Map<string, MockToolDef>();
        const { ctx: pluginCtx } = ctx(() => ({
          success: true,
          status,
          exit_code: exitCode,
          mode: "pty",
          output_incomplete: true,
          status_reason: reason,
          pty_screen: "captured prefix",
          pty_raw: "captured prefix",
        }));
        registerBashTool(api(tools), pluginCtx);
        const result = await tools
          .get("bash_status")!
          .execute(
            "call",
            { task_id: "bash-incomplete", output_mode: outputMode },
            undefined,
            undefined,
            { cwd: process.cwd() },
          );
        expect(text(result)).toContain(`Task bash-incomplete: ${status} (exit ${exitCode})`);
        expect(text(result)).toContain(reason);
        expect((result as { details: Record<string, unknown> }).details.output_incomplete).toBe(
          true,
        );
      }
    }
  });

  test("pty true implies background true (no explicit flag needed)", async () => {
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      task_id: "bash-pty-implied-bg",
      output:
        'PTY task started: bash-pty-implied-bg. Use bash_status({ taskId: "bash-pty-implied-bg", outputMode: "screen" }) to see the visible terminal, bash_write({ taskId: "bash-pty-implied-bg", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.',
    }));
    registerBashTool(api(tools), pluginCtx);
    // Caller omits background: true — plugin must auto-promote because pty:true
    // requires the polling background lifecycle.
    const result = await tools
      .get("bash")!
      .execute("call", { command: "python", pty: true }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("bash-pty-implied-bg");
    // Rust spawn payload sees background:true and pty:true.
    expect(calls.at(-1)?.[1]).toMatchObject({ pty: true, background: true });
  });

  test("bash pty true forwards pty to bridge", async () => {
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      task_id: "bash-pty",
      output:
        'PTY task started: bash-pty. Use bash_status({ taskId: "bash-pty", outputMode: "screen" }) to see the visible terminal, bash_write({ taskId: "bash-pty", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.',
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash")!
      .execute("call", { command: "python", pty: true, background: true }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("bash-pty");
    expect(calls[0][0]).toBe("bash");
    expect(calls[0][1]).toMatchObject({ pty: true, background: true });
  });

  test("bash pty dimensions are forwarded when pty:true and silently ignored when pty:false", async () => {
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx((_command, params) => ({
      success: true,
      status: "running",
      task_id: "bash-pty-dims",
      output: params.pty
        ? 'PTY task started: bash-pty-dims. Use bash_status({ taskId: "bash-pty-dims", outputMode: "screen" }) to see the visible terminal, bash_write({ taskId: "bash-pty-dims", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.'
        : "Background task started: bash-pty-dims. A completion reminder will be delivered automatically; don't poll bash_status.",
    }));
    registerBashTool(api(tools), pluginCtx);

    // pty:false + ptyRows passed defensively: should NOT throw, dims silently ignored
    const nonPtyResult = await tools
      .get("bash")!
      .execute("call", { command: "top", background: true, ptyRows: 50 }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(nonPtyResult)).toContain("bash-pty-dims");

    const result = await tools
      .get("bash")!
      .execute(
        "call",
        { command: "top", pty: true, background: true, ptyRows: 50, ptyCols: 120 },
        undefined,
        undefined,
        { cwd: process.cwd() },
      );
    expect(text(result)).toContain("bash-pty-dims");
    expect(calls.at(-1)?.[1]).toMatchObject({ pty_rows: 50, pty_cols: 120 });
  });

  test("bash_write schema accepts task_id/input and returns bridge response", async () => {
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({ success: true, bytes_written: 3 }));
    registerBashTool(api(tools), pluginCtx);
    const bashWrite = tools.get("bash_write")!;
    expect(JSON.stringify(bashWrite.parameters)).toContain("task_id");
    expect(JSON.stringify(bashWrite.parameters)).toContain("input");
    const result = await bashWrite.execute(
      "call",
      { task_id: "bash-abc", input: "x\n" },
      undefined,
      undefined,
      { cwd: process.cwd() },
    );
    expect(text(result)).toContain('"bytes_written": 3');
    expect(calls[0][0]).toBe("bash_write");
    expect(calls[0][1]).toMatchObject({ task_id: "bash-abc", input: "x\n" });
  });

  test("bash_status output_mode raw returns raw bytes", async () => {
    const outputPath = await spill("raw\u001b[31m-bytes");
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-raw", output_mode: "raw" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("raw\u001b[31m-bytes");
    expect(calls[0][1].output_mode).toBe("raw");
  });

  test("bash_status output_mode screen returns server-rendered screen", async () => {
    const outputPath = await spill("raw bytes are not rendered in screen mode");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
      pty_screen: "hello\n    there",
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-screen", output_mode: "screen" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("hello");
    expect(text(result)).toContain("there");
    expect(text(result)).not.toContain("raw bytes are not rendered");
  });

  test("bash_status output_mode both combines server screen with raw bytes", async () => {
    const outputPath = await spill("raw\u001b[31m-bytes");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
      pty_screen: "left\nwide",
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-both", output_mode: "both" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain(String.raw`"screen": "left\nwide"`);
    expect(text(result)).toContain(String.raw`"raw": "raw\u001b[31m-bytes"`);
  });

  test("bash_status preserves full coordinated non-PTY preview", async () => {
    const tools = new Map<string, MockToolDef>();
    const preview = `BEGIN-${"x".repeat(2500)}-END`;
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pipes",
      output_preview: preview,
    }));
    registerBashTool(api(tools), pluginCtx);

    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-long-preview" }, undefined, undefined, {
        cwd: process.cwd(),
      });

    expect(text(result)).toContain(preview);
    expect(text(result)).toContain("-END");
  });

  test("bash_status PTY path ignores compressed output_preview", async () => {
    const tools = new Map<string, MockToolDef>();
    const outputPath = await spill("raw pty bytes");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      mode: "pty",
      output_path: outputPath,
      output_preview: "COMPRESSED PIPE PREVIEW SHOULD NOT RENDER",
    }));
    registerBashTool(api(tools), pluginCtx);

    const result = await tools
      .get("bash_status")!
      .execute(
        "call",
        { task_id: "bash-pty-raw-guard", output_mode: "raw" },
        undefined,
        undefined,
        { cwd: process.cwd() },
      );

    expect(text(result)).toContain("raw pty bytes");
    expect(text(result)).not.toContain("COMPRESSED PIPE PREVIEW");
  });

  test("bash_status raw rereads the full PTY output file", async () => {
    const outputPath = await spill("first");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    const status = tools.get("bash_status")!;
    await status.execute(
      "call",
      { task_id: "bash-raw-reread", output_mode: "raw" },
      undefined,
      undefined,
      {
        cwd: process.cwd(),
      },
    );
    await appendFile(outputPath, "second");
    const second = await status.execute(
      "call",
      { task_id: "bash-raw-reread", output_mode: "raw" },
      undefined,
      undefined,
      { cwd: process.cwd() },
    );
    expect(text(second)).toContain("firstsecond");
  });

  test("bash_watch PTY scan is independent from bash_status cursor", async () => {
    const outputPath = await spill("rea");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-pty-shared", output_mode: "raw" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    await appendFile(outputPath, "dy\n");

    const result = await tools
      .get("bash_watch")!
      .execute(
        "call",
        { task_id: "bash-pty-shared", pattern: "ready", timeout_ms: 1 },
        undefined,
        undefined,
        { cwd: process.cwd() },
      );

    expect(text(result)).toContain('matched "ready" at offset 0');
  });

  test("bash_watch PTY scan times out without a match", async () => {
    const outputPath = await spill("not yet\n");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);

    const result = await tools
      .get("bash_watch")!
      .execute(
        "call",
        { task_id: "bash-pty-timeout", pattern: "ready", timeout_ms: 1 },
        undefined,
        undefined,
        { cwd: process.cwd() },
      );

    expect(text(result)).toContain("timeout reached without match");
  });

  test("bash_watch PTY terminal status renders server screen", async () => {
    const outputPath = await spill("done\n");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pty",
      output_path: outputPath,
      pty_screen: "done",
    }));
    registerBashTool(api(tools), pluginCtx);

    const result = await tools
      .get("bash_watch")!
      .execute(
        "call",
        { task_id: "bash-pty-exited", pattern: "missing", timeout_ms: 50 },
        undefined,
        undefined,
        { cwd: process.cwd() },
      );

    expect(text(result)).toContain("done");
  });

  test("bash_status terminal PTY screen uses server-rendered text", async () => {
    const outputPath = await spill("done");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pty",
      output_path: outputPath,
      pty_screen: "done",
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-done", output_mode: "screen" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("done");
  });
});
