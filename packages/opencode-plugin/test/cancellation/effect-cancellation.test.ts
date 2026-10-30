import { describe, expect, test } from "bun:test";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { Effect, Fiber } from "effect";

import { forwardEffectAbort } from "../../src/cancellation/effect-abort.js";
import { callBashBridge, callBridge, callToolCall } from "../../src/tools/_shared.js";
import { executeV2Bash } from "../../src/tools/bash/executor.js";
import { projectV2Tool } from "../../src/tools/definitions/v2.js";

const LOCATION = {
  directory: "/work/project",
  project: { directory: "/work/project", canonical: "/work/canonical" },
};

function executionContext() {
  return {
    sessionID: "session-v2",
    messageID: "message-v2",
    agent: "fixture",
    progress: () => Effect.succeed(undefined),
  };
}

describe("Effect-owned cancellation", () => {
  test("forwards one interruption and ignores interruption after disposal", async () => {
    const controller = new AbortController();
    let sends = 0;
    const forwarder = forwardEffectAbort(controller.signal, async () => {
      sends += 1;
      throw new Error("transport disposed");
    });

    controller.abort();
    controller.abort();
    forwarder.dispose();
    await Promise.resolve();

    expect(sends).toBe(1);
    expect(forwarder.forwarded).toBe(true);

    const disposedController = new AbortController();
    const disposed = forwardEffectAbort(disposedController.signal, () => {
      sends += 1;
    });
    disposed.dispose();
    disposedController.abort();
    expect(sends).toBe(1);
  });

  test("threads the Effect signal through native and tool_call Rust requests", async () => {
    const controller = new AbortController();
    const daemonLog: string[] = [];
    const seenSignals: Array<AbortSignal | undefined> = [];
    let markStarted: (() => void) | undefined;
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    const bridge = {
      send: async (_command: string, _params: unknown, options: { abortSignal?: AbortSignal }) => {
        seenSignals.push(options.abortSignal);
        return await new Promise<Record<string, unknown>>((_resolve, reject) => {
          options.abortSignal?.addEventListener(
            "abort",
            () => {
              daemonLog.push("request_cancelled command=search");
              reject(new Error("request_cancelled"));
            },
            { once: true },
          );
          markStarted?.();
        });
      },
      toolCall: async (
        _sessionID: string | undefined,
        _name: string,
        _input: unknown,
        options: { abortSignal?: AbortSignal },
      ) => {
        seenSignals.push(options.abortSignal);
        return { success: true, text: "ok" };
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
      storageDir: "/isolated/storage",
    } as never;
    const runtime = { directory: process.cwd(), effectAbort: controller.signal };
    const search = callBridge(ctx, runtime, "search", { query: "slow query" });
    // Host lookups yield before send installs its listener; wait for the request,
    // not an arbitrary microtask, so this exercises cancellation in flight.
    await started;
    controller.abort();

    await expect(search).rejects.toThrow("request_cancelled");
    await callToolCall(ctx, runtime, "outline", { target: "." });
    expect(seenSignals).toEqual([controller.signal, controller.signal]);
    expect(daemonLog).toEqual(["request_cancelled command=search"]);
  });

  test("cancels aft_search from its projected Effect fiber", async () => {
    const daemonLog: string[] = [];
    let markStarted: (() => void) | undefined;
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    const bridge = {
      send: async (_command: string, _params: unknown, options: { abortSignal?: AbortSignal }) =>
        await new Promise<Record<string, unknown>>((_resolve, reject) => {
          options.abortSignal?.addEventListener(
            "abort",
            () => {
              daemonLog.push("request_cancelled command=search");
              reject(new Error("request_cancelled"));
            },
            { once: true },
          );
          markStarted?.();
        }),
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
      storageDir: "/isolated/storage",
    } as never;
    const definition: ToolDefinition = {
      description: "long aft_search probe",
      args: {},
      execute: async (_input, runtime) => {
        await callBridge(ctx, runtime, "search", { query: "long search" });
        return "unexpected completion";
      },
    };
    const directory = process.cwd();
    const tool = projectV2Tool("aft_search", definition, {
      directory,
      project: { directory, canonical: directory },
    });
    const fiber = Effect.runFork(tool.execute({}, { progress: () => Effect.succeed(undefined) }));
    await started;

    await Effect.runPromise(Fiber.interruptAll([fiber]));

    expect(daemonLog).toEqual(["request_cancelled command=search"]);
  });

  test("honors explicit opt-out of the V1 ToolContext abort signal", async () => {
    const controller = new AbortController();
    const seenSignals: Array<AbortSignal | undefined> = [];
    const bridge = {
      send: async (_command: string, _params: unknown, options: { abortSignal?: AbortSignal }) => {
        seenSignals.push(options.abortSignal);
        return { success: true };
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
      storageDir: "/isolated/storage",
    } as never;

    await callBridge(
      ctx,
      { directory: process.cwd(), abort: controller.signal } as never,
      "status",
      {},
      { abortSignal: undefined },
    );
    expect(seenSignals).toEqual([undefined]);
  });

  test("keeps generic cancel_request off the foreground bash path", async () => {
    const controller = new AbortController();
    const seenSignals: Array<AbortSignal | undefined> = [];
    const bridge = {
      send: async (_command: string, _params: unknown, options: { abortSignal?: AbortSignal }) => {
        seenSignals.push(options.abortSignal);
        return { success: true, output: "done" };
      },
    };
    const ctx = {
      pool: { getBridge: () => bridge },
      client: {},
      config: {},
      storageDir: "/isolated/storage",
    } as never;

    await callBashBridge(
      ctx,
      { directory: process.cwd(), effectAbort: controller.signal },
      "bash",
      {
        command: "true",
      },
    );
    expect(seenSignals).toEqual([undefined]);
  });

  test("maps a foreground host interruption only to bash_abort_inflight", async () => {
    const host = new AbortController();
    const rustCommands: string[] = [];
    const definition: ToolDefinition = {
      description: "foreground bash probe",
      args: {},
      execute: async (_input, context) =>
        await new Promise<string>((resolve) => {
          context.abort.addEventListener(
            "abort",
            () => {
              rustCommands.push("bash_abort_inflight");
              resolve("call_aborted");
            },
            { once: true },
          );
        }),
    };

    const resultPromise = executeV2Bash({
      name: "bash",
      input: { command: "sleep 30", wait: true },
      context: {
        ...executionContext(),
        directory: LOCATION.directory,
        worktree: LOCATION.project.canonical,
        abort: host.signal,
        metadata: () => {},
        ask: async () => {},
      },
      definition,
    });

    host.abort();
    expect(await resultPromise).toBe("call_aborted");
    expect(rustCommands).toEqual(["bash_abort_inflight"]);
    expect(rustCommands).not.toContain("bash_wait_detach");
  });

  test("Effect interruption fires bash abort before the host records aborted", async () => {
    const events: string[] = [];
    let markStarted: (() => void) | undefined;
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    const definition: ToolDefinition = {
      description: "interrupt ordering probe",
      args: {},
      execute: async (_input, context) =>
        await new Promise<string>(() => {
          context.abort.addEventListener(
            "abort",
            () => {
              events.push("send:bash_abort_inflight");
            },
            { once: true },
          );
          markStarted?.();
        }),
    };
    const tool = projectV2Tool("bash", definition, LOCATION, {
      executeBash: executeV2Bash,
    });
    const fiber = Effect.runFork(
      tool.execute({ command: "sleep 30", wait: true }, executionContext()),
    );
    await started;

    const interruptedAt = Date.now();
    await Effect.runPromise(Fiber.interruptAll([fiber]));
    events.push("history:aborted");

    expect(Date.now() - interruptedAt).toBeLessThan(250);
    expect(events).toEqual(["send:bash_abort_inflight", "history:aborted"]);
  });

  test("explicit background execution is settled before later host interruption", async () => {
    const host = new AbortController();
    let downstreamAborts = 0;
    const definition: ToolDefinition = {
      description: "background bash probe",
      args: {},
      execute: async (_input, context) => {
        context.abort.addEventListener("abort", () => {
          downstreamAborts += 1;
        });
        return "bash-task-1";
      },
    };

    const result = await executeV2Bash({
      name: "bash",
      input: { command: "sleep 1", background: true },
      context: {
        ...executionContext(),
        directory: LOCATION.directory,
        worktree: LOCATION.project.canonical,
        abort: host.signal,
        metadata: () => {},
        ask: async () => {},
      },
      definition,
    });
    host.abort();

    expect(result).toBe("bash-task-1");
    expect(downstreamAborts).toBe(0);
  });
});
