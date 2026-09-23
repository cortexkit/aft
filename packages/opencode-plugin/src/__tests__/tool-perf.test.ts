import { afterAll, beforeEach, describe, expect, mock, test } from "bun:test";
import type { ToolDefinition } from "@opencode-ai/plugin";

// Full enumeration of logger exports so the partial mock can't leak missing
// members into other suites (see the bun mock.module leakage rule).
const sessionLogSpy = mock((_sessionId: string | undefined, _message: string) => {});
mock.module("../logger.js", () => ({
  log: () => {},
  debug: () => {},
  warn: () => {},
  error: () => {},
  sessionLog: sessionLogSpy,
  sessionDebug: () => {},
  sessionWarn: () => {},
  sessionError: () => {},
  getLogFilePath: () => "",
  flushLog: () => {},
  bridgeLogger: { log: () => {}, warn: () => {}, error: () => {} },
}));

const { instrumentToolMap, markBridgeStart, markBridgeEnd } = await import("../tool-perf.js");

function makeTool(execute: ToolDefinition["execute"]): ToolDefinition {
  return { description: "t", args: {}, execute } as ToolDefinition;
}

function lastLine(): { sessionId: string | undefined; message: string } | undefined {
  const calls = sessionLogSpy.mock.calls;
  if (calls.length === 0) return undefined;
  const [sessionId, message] = calls[calls.length - 1] as [string | undefined, string];
  return { sessionId, message };
}

describe("tool-perf instrumentation", () => {
  beforeEach(() => {
    sessionLogSpy.mockClear();
  });

  afterAll(() => {
    mock.restore();
  });

  test("logs total + session + tool name on a pure-TS tool (no bridge call)", async () => {
    const tools = instrumentToolMap({
      sample: makeTool(async () => "ok"),
    });
    const result = await tools.sample!.execute!({} as never, { sessionID: "ses_123" } as never);
    expect(result).toBe("ok");

    const line = lastLine();
    expect(line?.sessionId).toBe("ses_123");
    expect(line?.message).toContain("perf tool=sample");
    expect(line?.message).toMatch(/total=\d+ms/);
    expect(line?.message).toContain("(no bridge call)");
  });

  test("breaks latency into pre/bridge/post when the tool marks a bridge window", async () => {
    const tools = instrumentToolMap({
      bridged: makeTool(async () => {
        // Simulate the callBridge marks bracketing the bridge round-trip.
        markBridgeStart();
        markBridgeEnd();
        return "done";
      }),
    });
    await tools.bridged!.execute!({} as never, { sessionID: "ses_x" } as never);

    const line = lastLine();
    expect(line?.message).toContain("perf tool=bridged");
    expect(line?.message).toMatch(/total=\d+ms/);
    expect(line?.message).toMatch(/pre=\d+ms/);
    expect(line?.message).toMatch(/bridge=\d+ms/);
    expect(line?.message).toMatch(/post=\d+ms/);
    expect(line?.message).not.toContain("(no bridge call)");
  });

  test("emits a perf line even when the tool throws", async () => {
    const tools = instrumentToolMap({
      boom: makeTool(async () => {
        throw new Error("kaboom");
      }),
    });
    await expect(
      tools.boom!.execute!({} as never, { sessionID: "ses_e" } as never),
    ).rejects.toThrow("kaboom");

    const line = lastLine();
    expect(line?.message).toContain("perf tool=boom");
  });

  test("first bridgeStart and last bridgeEnd win across multiple bridge calls", async () => {
    const tools = instrumentToolMap({
      multi: makeTool(async () => {
        markBridgeStart();
        markBridgeStart(); // ignored — first wins
        markBridgeEnd();
        markBridgeEnd(); // last wins
        return "ok";
      }),
    });
    await tools.multi!.execute!({} as never, { sessionID: undefined } as never);

    const line = lastLine();
    expect(line?.sessionId).toBeUndefined();
    expect(line?.message).toContain("perf tool=multi");
    expect(line?.message).toMatch(/bridge=\d+ms/);
  });

  test("marks outside a tool invocation are no-ops (no crash, no log)", () => {
    // No AsyncLocalStorage context established — must not throw.
    expect(() => {
      markBridgeStart();
      markBridgeEnd();
    }).not.toThrow();
  });
});
