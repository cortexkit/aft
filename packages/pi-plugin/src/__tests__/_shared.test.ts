/**
 * Unit tests for shared Pi tool bridge helpers.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Type } from "typebox";
import { Value } from "typebox/value";
import {
  bridgeFor,
  callBridge,
  callToolCall,
  jsonTextResult,
  optionalInt,
  stripSuccess,
  textResult,
} from "../tools/_shared.js";
import { makeExtContext, makeMockBridge } from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("tool shared helpers", () => {
  test("bridgeFor resolves the bridge using the current cwd", () => {
    const { bridge } = makeMockBridge();
    const requested: string[] = [];
    const ctx = {
      pool: {
        getBridge(cwd: string) {
          requested.push(cwd);
          return bridge;
        },
      },
    } as never;

    expect(bridgeFor(ctx, projectRoot)).toBe(bridge);
    expect(requested).toEqual([projectRoot]);
  });

  test("callBridge caps every synchronous transport request below Pi's hard deadline", async () => {
    const { bridge, calls } = makeMockBridge((_command, params) => ({ success: true, params }));
    const extCtx = makeExtContext(projectRoot, "pi-session-123");

    const response = await callBridge(bridge, "grep", { pattern: "needle" }, extCtx);

    expect(response.params).toEqual({ pattern: "needle", session_id: "pi-session-123" });
    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("grep");
    expect(calls[0].params).toEqual({ pattern: "needle", session_id: "pi-session-123" });
    expect(calls[0].options?.timeoutMs).toBe(25_000);
    expect(calls[0].options?.transportTimeoutMs).toBe(25_000);
    expect(calls[0].options?.executionDeadlineMs).toBe(24_000);
    expect(calls[0].options?.configureWarningClient).toBe(extCtx);
  });

  test("callBridge caps explicit transport options at the Pi deadline", async () => {
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));

    await callBridge(bridge, "bash", { command: "sleep 60" }, makeExtContext(), {
      transportTimeoutMs: 70_000,
      keepBridgeOnTimeout: true,
    });

    expect(calls[0].options?.transportTimeoutMs).toBe(25_000);
    expect(calls[0].options?.executionDeadlineMs).toBe(24_000);
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
    expect(calls[0].options?.configureWarningClient).toBeDefined();
  });

  test("callToolCall propagates raw agent args, session id, preview flag, and timeout", async () => {
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      text: "ok",
      params,
    }));
    const extCtx = makeExtContext(projectRoot, "pi-session-123");

    const response = await callToolCall(
      bridge,
      "edit",
      { filePath: "a.ts", oldString: "a", newString: "b" },
      extCtx,
      { preview: true },
    );

    expect(response.params).toEqual({ filePath: "a.ts", oldString: "a", newString: "b" });
    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toEqual({
      name: "edit",
      arguments: { filePath: "a.ts", oldString: "a", newString: "b" },
      session_id: "pi-session-123",
      preview: true,
    });
    expect(calls[0].options?.configureWarningClient).toBe(extCtx);
    expect(calls[0].options?.executionDeadlineMs).toBe(24_000);
  });

  test("callToolCall emits visible progress while a tool remains pending", async () => {
    let release!: () => void;
    const pending = new Promise<void>(resolve => {
      release = resolve;
    });
    const updates: unknown[] = [];
    const { bridge } = makeMockBridge(async () => {
      await pending;
      return { success: true, text: "ok" };
    });

    const call = callToolCall(bridge, "inspect", {}, makeExtContext(), {
      onUpdate: update => updates.push(update),
      progressIntervalMs: 5,
    });
    await Bun.sleep(12);
    release();
    await call;

    expect(updates.length).toBeGreaterThan(0);
  });

  test("callBridge throws Rust error messages instead of exposing failure payloads", async () => {
    const { bridge } = makeMockBridge(() => ({ success: false, message: "bad request" }));

    await expect(callBridge(bridge, "outline", {}, makeExtContext())).rejects.toThrow(
      "bad request",
    );
  });

  test("text helpers preserve agent-facing text and strip success metadata", () => {
    expect(textResult("hello", { ok: true })).toEqual({
      content: [{ type: "text", text: "hello" }],
      details: { ok: true },
    });
    expect(jsonTextResult({ success: true, file: "a.ts" }).content[0].text).toContain(
      '"success": true',
    );
    expect(stripSuccess({ success: true, file: "a.ts" })).toEqual({ file: "a.ts" });
  });
});

describe("optionalInt", () => {
  test("accepts missing values, integers, and stringified integers", () => {
    const schema = Type.Object({
      startLine: optionalInt(1, 100, "1-based start line"),
    });

    expect(Value.Check(schema, {})).toBe(true);
    expect(Value.Check(schema, { startLine: 24 })).toBe(true);
    expect(Value.Check(schema, { startLine: "24" })).toBe(true);
    expect(Value.Check(schema, { startLine: 24.5 })).toBe(false);
    expect(Value.Check(schema, { startLine: null })).toBe(false);
  });

  test("keeps unions at the field level with documented integer bounds", () => {
    const schema = Type.Object({
      startLine: optionalInt(1, 60, "1-based start line"),
    }) as {
      type?: unknown;
      anyOf?: unknown;
      properties?: {
        startLine?: {
          description?: unknown;
          anyOf?: Array<Record<string, unknown>>;
        };
      };
    };

    expect(schema.type).toBe("object");
    expect(schema.anyOf).toBeUndefined();

    const startLineSchema = schema.properties?.startLine;
    expect(startLineSchema?.description).toBe("1-based start line");
    expect(Array.isArray(startLineSchema?.anyOf)).toBe(true);

    const variants = startLineSchema?.anyOf ?? [];
    expect(
      variants.some(
        (variant) => variant.type === "integer" && variant.minimum === 1 && variant.maximum === 60,
      ),
    ).toBe(true);
    expect(variants.some((variant) => variant.type === "string")).toBe(true);
  });
});
