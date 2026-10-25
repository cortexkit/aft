import { afterAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AftTransportPool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { Effect } from "effect";
import * as configModule from "../config.js";
import { semanticTools } from "../tools/semantic.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

const root = mkdtempSync(join(tmpdir(), "aft-gather-context-"));
const external = mkdtempSync(join(tmpdir(), "aft-gather-external-"));
afterAll(() => {
  rmSync(root, { recursive: true, force: true });
  rmSync(external, { recursive: true, force: true });
});

type Version = 1 | 2;
type Part = Record<string, unknown>;
type FixtureOptions = {
  enabled?: boolean;
  version: Version;
  parts?: Part[];
  missingMessage?: boolean;
  missingSession?: boolean;
  callID?: string;
  reject?: "message" | "list";
  ask?: ToolContext["ask"];
  read?: (sessionID: string, signal?: AbortSignal) => Promise<void>;
  abort?: AbortSignal;
};

function partsFor(version: Version, sessionID: string): Part[] {
  return [
    { type: "reasoning", text: `reasoning-${sessionID}` },
    { type: "text", text: `visible-${sessionID}` },
    { type: "file", text: "attachment-secret", url: "file:///private" },
    {
      type: "tool",
      ...(version === 1
        ? { callID: "call-current", tool: "aft_search" }
        : { id: "call-current", name: "aft_search" }),
      state: { status: "running", input: { query: "argument-secret" } },
    },
    { type: "text", text: "later-text-secret" },
    {
      type: "tool",
      ...(version === 1 ? { callID: "later", tool: "read" } : { id: "later", name: "read" }),
      state: { status: "completed", output: "result-secret" },
    },
  ];
}

let nextSession = 0;
function openCodeGatherFixture(options: FixtureOptions) {
  const defaultSession = `gather-${++nextSession}`;
  const calls: Array<{
    sessionID: string | undefined;
    rawArgs: Record<string, unknown>;
    options?: Record<string, unknown>;
  }> = [];
  const messageInputs: unknown[] = [];
  const listInputs: unknown[] = [];
  const readSignals: Array<AbortSignal | undefined> = [];
  let readerConstructions = 0;
  let effectRuns = 0;
  const message = async (sessionID: string, signal?: AbortSignal) => {
    if (options.read) await options.read(sessionID, signal);
    if (options.reject === "message") throw new Error("host unavailable");
    const parts = options.parts ?? partsFor(options.version, sessionID);
    return options.version === 1
      ? { info: { id: "current-message", role: "assistant", sessionID }, parts }
      : { id: "current-message", type: "assistant", content: parts };
  };
  const bridge = {
    toolCall: async (
      sessionID: string | undefined,
      _name: string,
      rawArgs: Record<string, unknown>,
      sendOptions?: Record<string, unknown>,
    ) => {
      expect(rawArgs).not.toHaveProperty("search_context");
      expect(rawArgs).not.toHaveProperty("searchContext");
      calls.push({ sessionID, rawArgs, options: sendOptions });
      return { success: true, text: "search-result" };
    },
  };
  const client = {
    session: {
      get: async () => ({ data: { directory: root } }),
      get message() {
        readerConstructions++;
        if (options.version === 2) throw new Error("V1 reader used on V2");
        return async (input: { path: { id: string; messageID: string }; signal?: AbortSignal }) => {
          messageInputs.push({ path: input.path });
          readSignals.push(input.signal);
          return { data: await message(input.path.id, input.signal) };
        };
      },
      messages: async (input: {
        path: { id: string };
        query: { limit: number };
        signal?: AbortSignal;
      }) => {
        listInputs.push({ path: input.path, query: input.query });
        readSignals.push(input.signal);
        expect(input.query.limit).toBeGreaterThan(0);
        expect(input.query.limit).toBeLessThanOrEqual(50);
        if (options.reject === "list") throw new Error("history unavailable");
        return {
          data: [
            { info: { role: "user" }, parts: [{ type: "text", text: "old-user-secret" }] },
            {
              info: { role: "assistant" },
              parts: [{ type: "reasoning", text: "old-assistant-secret" }],
            },
            {
              info: { role: "user" },
              parts: [
                { type: "text", text: `user-${input.path.id}` },
                { type: "file", text: "user-attachment-secret" },
              ],
            },
            await message(input.path.id, input.signal),
          ],
        };
      },
      context: () => {
        throw new Error("unbounded V2 context read");
      },
    },
  };
  const ctx: PluginContext = {
    pool: { getBridge: () => bridge } as unknown as AftTransportPool,
    client: client as unknown as PluginContext["client"],
    config: { search: { gather: { enabled: options.enabled } } },
    storageDir: root,
    ...(options.version === 2
      ? {
          readSearchMessageV2: (input: { sessionID: string; messageID: string }) => {
            readerConstructions++;
            return Effect.tryPromise(async (signal) => {
              effectRuns++;
              messageInputs.push(input);
              return message(input.sessionID, signal);
            });
          },
        }
      : {}),
  };
  const tools = semanticTools(ctx);
  const search = (
    args: Record<string, unknown> = { query: "how is config loaded" },
    sessionID = defaultSession,
  ) => {
    const runtime = {
      directory: root,
      worktree: root,
      sessionID: options.missingSession ? undefined : sessionID,
      messageID: options.missingMessage ? undefined : "current-message",
      ...(options.version === 2
        ? { hostVersion: 2, id: options.callID ?? "call-current", effectAbort: options.abort }
        : {}),
      ...(options.version === 1 && options.callID ? { callID: options.callID } : {}),
      abort: options.abort ?? new AbortController().signal,
      agent: "test",
      metadata: () => {},
      ask: options.ask ?? noopAsk,
    };
    return tools.aft_search.execute(args, runtime as ToolContext);
  };
  return {
    ctx,
    calls,
    search,
    defaultSession,
    messageInputs,
    listInputs,
    readSignals,
    effectRuns: () => effectRuns,
    gatherMessageReads: () => messageInputs.length + listInputs.length,
    readerConstructions: () => readerConstructions,
    sentOptions: () => calls.at(-1)?.options ?? {},
    trace: () => calls.at(-1)?.options?.searchContext,
  };
}

for (const version of [1, 2] as const) {
  for (const enabled of [false, undefined]) {
    test(`V${version} gather ${String(enabled)} constructs no readers and reads no parts`, async () => {
      const f = openCodeGatherFixture({ enabled, version });
      await f.search();
      expect(f.calls).toHaveLength(1);
      expect(f.gatherMessageReads()).toBe(0);
      expect(f.readerConstructions()).toBe(0);
      expect(f.sentOptions()).not.toHaveProperty("searchContext");
    });
  }

  test(`V${version} gathers only current preceding reasoning and visible text`, async () => {
    const f = openCodeGatherFixture({ enabled: true, version });
    await f.search({ query: "how is config loaded", pattern: "load_config" });
    expect(f.trace()).toContain(`reasoning-${f.defaultSession}`);
    expect(f.trace()).toContain(`visible-${f.defaultSession}`);
    if (version === 1) {
      expect(f.trace()).toContain(`user-${f.defaultSession}`);
      expect(f.messageInputs).toEqual([
        { path: { id: f.defaultSession, messageID: "current-message" } },
      ]);
      expect(f.listInputs).toHaveLength(1);
    } else {
      expect(f.effectRuns()).toBe(1);
      expect(f.messageInputs).toEqual([
        { sessionID: f.defaultSession, messageID: "current-message" },
      ]);
      expect(f.listInputs).toHaveLength(0);
    }
    expect(f.trace()).not.toContain("secret");
    expect(f.calls[0].rawArgs).toEqual({ query: "how is config loaded", pattern: "load_config" });
  });

  test(`V${version} empty reasoning still collects visible text`, async () => {
    const parts = partsFor(version, "empty");
    parts[0].text = "";
    const f = openCodeGatherFixture({ enabled: true, version, parts });
    await f.search();
    expect(f.trace()).toContain("visible-empty");
    expect(f.trace()).not.toContain("secret");
  });

  test(`V${version} drops whitespace-only parts`, async () => {
    const parts = partsFor(version, "spaces");
    parts[0].text = " \t\n";
    parts[1].text = "kept";
    const f = openCodeGatherFixture({ enabled: true, version, parts });
    await f.search();
    expect(f.trace()).toBe(version === 1 ? `kept\n\nuser-${f.defaultSession}` : "kept");
  });

  for (const missing of ["missingMessage", "missingSession"] as const) {
    test(`V${version} ${missing} reads no parts`, async () => {
      const f = openCodeGatherFixture({ enabled: true, version, [missing]: true });
      await f.search();
      expect(f.gatherMessageReads()).toBe(0);
      expect(f.sentOptions()).not.toHaveProperty("searchContext");
    });
  }

  test(`V${version} unproved tool boundary omits context`, async () => {
    const f = openCodeGatherFixture({
      enabled: true,
      version,
      parts: [{ type: "text", text: "unsafe" }],
    });
    await f.search();
    expect(f.messageInputs).toHaveLength(1);
    expect(f.listInputs).toHaveLength(0);
    expect(f.sentOptions()).not.toHaveProperty("searchContext");
  });

  test(`V${version} SDK rejection keeps search query-only`, async () => {
    const f = openCodeGatherFixture({ enabled: true, version, reject: "message" });
    expect(await f.search()).toBe("search-result");
    expect(f.messageInputs).toHaveLength(1);
    expect(f.sentOptions()).not.toHaveProperty("searchContext");
  });

  test(`V${version} caps the joined trace to newest 6000 Unicode code points`, async () => {
    const parts = partsFor(version, "unicode");
    parts[0].text = `discard${"界🙂".repeat(3500)}`;
    const f = openCodeGatherFixture({ enabled: true, version, parts });
    await f.search();
    const trace = f.trace();
    expect(typeof trace).toBe("string");
    expect(Array.from(trace as string)).toHaveLength(6000);
    expect(trace).not.toContain("discard");
    expect(trace).not.toContain("\uFFFD");
    expect(trace).toContain("visible-unicode");
    if (version === 1) expect(trace).toContain(`user-${f.defaultSession}`);
  });

  test(`V${version} reads current config after a live disable`, async () => {
    const f = openCodeGatherFixture({ enabled: true, version });
    await f.search();
    expect(f.trace()).toContain(`visible-${f.defaultSession}`);
    const reads = f.gatherMessageReads();
    f.ctx.config = { search: { gather: { enabled: false } } };
    await f.search();
    expect(f.gatherMessageReads()).toBe(reads);
    expect(f.sentOptions()).not.toHaveProperty("searchContext");
  });

  test(`V${version} permission rejection reads no parts`, async () => {
    const f = openCodeGatherFixture({
      enabled: true,
      version,
      ask: async () => {
        throw new Error("denied");
      },
    });
    await Promise.resolve(f.search()).catch(() => undefined);
    expect(f.calls).toHaveLength(0);
    expect(f.gatherMessageReads()).toBe(0);
  });

  test(`V${version} project-only permission fallback carries context in options`, async () => {
    const f = openCodeGatherFixture({
      enabled: true,
      version,
      ask: async (input) => {
        if (input.permission !== "aft_search_external") return;
        const error = Object.assign(new Error("external rule denied"), { _tag: "DeniedError" });
        throw error;
      },
    });
    const result = await f.search({ query: "how is config loaded", path: external });
    expect(typeof result === "string" ? result : result.output).toContain(
      "searching the project index only",
    );
    expect(f.trace()).toContain(`visible-${f.defaultSession}`);
    expect(f.calls[0].rawArgs).toEqual({ query: "how is config loaded" });
  });

  test(`V${version} concurrent sessions do not share context`, async () => {
    let entered!: () => void;
    let release!: () => void;
    const firstEntered = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const released = new Promise<void>((resolve) => {
      release = resolve;
    });
    const f = openCodeGatherFixture({
      enabled: true,
      version,
      read: async (id) => {
        if (id === "session-a") {
          entered();
          await released;
        }
      },
    });
    const a = f.search(undefined, "session-a");
    await firstEntered;
    await f.search(undefined, "session-b");
    release();
    await a;
    expect(f.calls).toHaveLength(2);
    for (const call of f.calls) {
      expect(call.options?.searchContext).toContain(`visible-${call.sessionID}`);
      expect(call.options?.searchContext).not.toContain(
        call.sessionID === "session-a" ? "session-b" : "session-a",
      );
    }
  });
}

test("V1 two parallel active searches have no proved boundary", async () => {
  const parts = partsFor(1, "ambiguous");
  parts.push({ type: "tool", callID: "other", tool: "aft_search", state: { status: "pending" } });
  const f = openCodeGatherFixture({ enabled: true, version: 1, parts });
  await f.search();
  expect(f.messageInputs).toHaveLength(1);
  expect(f.listInputs).toHaveLength(0);
  expect(f.sentOptions()).not.toHaveProperty("searchContext");
});

test("V1 explicit call id selects its exact tool boundary", async () => {
  const parts = partsFor(1, "explicit");
  parts.push({ type: "tool", callID: "other", tool: "aft_search", state: { status: "pending" } });
  const f = openCodeGatherFixture({ enabled: true, version: 1, callID: "call-current", parts });
  await f.search();
  expect(f.trace()).toContain("visible-explicit");
  expect(f.trace()).not.toContain("secret");
});

test("V1 bounded user-message read failure keeps search query-only", async () => {
  const f = openCodeGatherFixture({ enabled: true, version: 1, reject: "list" });
  expect(await f.search()).toBe("search-result");
  expect(f.messageInputs).toHaveLength(1);
  expect(f.listInputs).toHaveLength(1);
  expect(f.sentOptions()).not.toHaveProperty("searchContext");
});

for (const version of [1, 2] as const) {
  test(`V${version} an aborted call constructs no gather readers`, async () => {
    const controller = new AbortController();
    controller.abort();
    const f = openCodeGatherFixture({ enabled: true, version, abort: controller.signal });
    await f.search();
    expect(f.gatherMessageReads()).toBe(0);
    expect(f.readerConstructions()).toBe(0);
    expect(f.sentOptions()).not.toHaveProperty("searchContext");
  });

  test(`V${version} abort during a read stops history and context`, async () => {
    const controller = new AbortController();
    let entered!: () => void;
    let release!: () => void;
    const reading = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const released = new Promise<void>((resolve) => {
      release = resolve;
    });
    const f = openCodeGatherFixture({
      enabled: true,
      version,
      abort: controller.signal,
      read: async () => {
        entered();
        await released;
      },
    });
    const pending = f.search();
    await reading;
    controller.abort();
    release();
    await pending;
    expect(f.messageInputs).toHaveLength(1);
    expect(f.listInputs).toHaveLength(0);
    expect(f.sentOptions()).not.toHaveProperty("searchContext");
    if (version === 1) expect(f.readSignals[0]?.aborted).toBe(true);
  });

  test(`V${version} a late timeout loser rejection has a handler`, async () => {
    let rejectLate!: (reason: Error) => void;
    const late = new Promise<void>((_resolve, reject) => {
      rejectLate = reject;
    });
    const unhandled: unknown[] = [];
    const onUnhandled = (reason: unknown) => {
      unhandled.push(reason);
    };
    process.on("unhandledRejection", onUnhandled);
    try {
      const f = openCodeGatherFixture({ enabled: true, version, read: () => late });
      expect(await f.search()).toBe("search-result");
      expect(f.messageInputs).toHaveLength(1);
      expect(f.listInputs).toHaveLength(0);
      expect(f.sentOptions()).not.toHaveProperty("searchContext");
      rejectLate(new Error("late-reader-rejection"));
      await new Promise<void>((resolve) => setImmediate(resolve));
      await new Promise<void>((resolve) => setImmediate(resolve));
      expect(unhandled).toEqual([]);
    } finally {
      process.off("unhandledRejection", onUnhandled);
    }
  });
}

test("V1 both SDK reads receive the tool cancellation signal", async () => {
  const controller = new AbortController();
  const f = openCodeGatherFixture({ enabled: true, version: 1, abort: controller.signal });
  await f.search();
  expect(f.readSignals).toHaveLength(2);
  controller.abort();
  expect(f.readSignals.map((signal) => signal?.aborted)).toEqual([true, true]);
});

test("gatherEnabled requires explicit enablement in effective config", () => {
  expect(configModule.gatherEnabled).toBeFunction();
  for (const [config, enabled] of [
    [{}, false],
    [{ search: {} }, false],
    [{ search: { gather: {} } }, false],
    [{ search: { gather: { enabled: false } } }, false],
    [{ search: { gather: { enabled: true } } }, true],
  ] as const) {
    expect(configModule.gatherEnabled(config)).toBe(enabled);
  }
});
