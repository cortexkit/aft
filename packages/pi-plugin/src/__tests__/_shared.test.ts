/**
 * Unit tests for shared Pi tool bridge helpers.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  bridgeFor,
  callBridge,
  jsonTextResult,
  stripSuccess,
  textResult,
} from "../tools/_shared.js";
import { makeExtContext, makeMockBridge } from "./tool-test-utils.js";

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

    expect(bridgeFor(ctx, "/workspace/project")).toBe(bridge);
    expect(requested).toEqual(["/workspace/project"]);
  });

  test("callBridge propagates session id, warning client, and long-command timeout", async () => {
    const { bridge, calls } = makeMockBridge((_command, params) => ({ success: true, params }));
    const extCtx = makeExtContext("/repo", "pi-session-123");

    const response = await callBridge(bridge, "grep", { pattern: "needle" }, extCtx);

    expect(response.params).toEqual({ pattern: "needle", session_id: "pi-session-123" });
    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("grep");
    expect(calls[0].params).toEqual({ pattern: "needle", session_id: "pi-session-123" });
    expect(calls[0].options?.timeoutMs).toBe(60_000);
    expect(calls[0].options?.configureWarningClient).toBe(extCtx);
  });

  test("callBridge keeps explicit transport options while preserving default timeout", async () => {
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));

    await callBridge(bridge, "bash", { command: "sleep 60" }, makeExtContext(), {
      transportTimeoutMs: 70_000,
      keepBridgeOnTimeout: true,
    });

    expect(calls[0].options?.transportTimeoutMs).toBe(70_000);
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
    expect(calls[0].options?.configureWarningClient).toBeDefined();
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
