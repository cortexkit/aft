/// <reference path="../bun-test.d.ts" />

/**
 * A request timeout settles only its own request; restarting a hung child is
 * the hang watchdog's job. The watchdog acts only when the child owes a reply
 * and has written nothing for its silence window: it then sends a `ping`, and
 * kills the child only when that goes unanswered. Calls cut off by that
 * restart get a typed error, and read-only tools are retried once on the
 * respawned child while mutations are not.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BinaryBridge } from "../bridge.js";
import type { Logger, LogMeta } from "../logger.js";
import { BridgePool } from "../pool.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";
import { ManualClock } from "./test-utils/manual-clock.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-hang-watchdog-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

/**
 * Fake engine that behaves like the real one where it matters here: it starts
 * only when the bridge first sends, reads requests in order, and can stay
 * silent. Every spawn appends to `spawns` in its working directory, so a
 * respawned engine knows it is generation 2, and every request is logged to
 * `requests.ndjson` with that generation.
 *
 * Tools (the `tool_call` name, or the direct command):
 * - `configure` / `version`: answered at once.
 * - `wedge`: in generation 1 the engine stops answering anything from then on,
 *   like a dispatcher stuck in one handler. Later generations answer it.
 * - `never`: never answered, but later requests still are, like one stuck
 *   worker in an otherwise healthy engine.
 * - `ping` (the hang watchdog's liveness probe): answered like anything
 *   else, except that while a `hold-pongs` file exists the answer is held
 *   until a `release-pong` file appears; once the answer is written to the
 *   pipe, the engine creates `pong-written`.
 * - anything else: answered with `{ tool, generation }`, after `delay_ms`
 *   when the request carries one, or once the file named by `until_file`
 *   exists in the working directory (without holding up later requests, and
 *   even after a `wedge`).
 */
const FAKE_ENGINE = `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const spawns = path.join(process.cwd(), "spawns");
fs.appendFileSync(spawns, "spawn\\n");
const generation = fs.readFileSync(spawns, "utf8").split("\\n").filter(Boolean).length;
const requestLog = path.join(process.cwd(), "requests.ndjson");
process.stdin.setEncoding("utf8");
let buffer = "";
let wedged = false;
function write(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const req = JSON.parse(buffer.slice(0, newline));
    buffer = buffer.slice(newline + 1);
    const tool = req.command === "tool_call" ? req.name : req.command;
    fs.appendFileSync(requestLog, JSON.stringify({ generation, id: req.id, tool }) + "\\n");
    if (wedged) continue;
    if (req.command === "configure" || req.command === "version") {
      write({ id: req.id, success: true, warnings: [], version: "99.0.0" });
      continue;
    }
    if (tool === "wedge" && generation === 1) {
      wedged = true;
      continue;
    }
    if (tool === "never") continue;
    if (tool === "ping" && fs.existsSync(path.join(process.cwd(), "hold-pongs"))) {
      const poll = setInterval(() => {
        if (!fs.existsSync(path.join(process.cwd(), "release-pong"))) return;
        clearInterval(poll);
        process.stdout.write(JSON.stringify({ id: req.id, success: true, command: "pong" }) + "\\n", () =>
          fs.writeFileSync(path.join(process.cwd(), "pong-written"), ""),
        );
      }, 5);
      continue;
    }
    const args = req.arguments || req.params || {};
    const reply = () => write({ id: req.id, success: true, tool, generation });
    if (args.until_file) {
      const poll = setInterval(() => {
        if (!fs.existsSync(path.join(process.cwd(), args.until_file))) return;
        clearInterval(poll);
        reply();
      }, 5);
    } else if (args.delay_ms > 0) setTimeout(reply, args.delay_ms);
    else reply();
  }
});
`;

/** Silence-window floor for the watchdog tests; see `wedgeThen`. */
const WINDOW_MS = 1_000;
/** Liveness-probe timeout for the watchdog tests; see `wedgeThen`. */
const PROBE_MS = 100;

interface LoggedRequest {
  generation: number;
  id: string;
  tool: string;
}

function loggedRequests(): LoggedRequest[] {
  const path = join(workDir, "requests.ndjson");
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as LoggedRequest);
}

function recordingLogger(): {
  logger: Logger;
  logs: Array<{ level: string; message: string; meta?: LogMeta }>;
} {
  const logs: Array<{ level: string; message: string; meta?: LogMeta }> = [];
  return {
    logs,
    logger: {
      log: (message, meta) => logs.push({ level: "info", message, meta }),
      warn: (message, meta) => logs.push({ level: "warn", message, meta }),
      error: (message, meta) => logs.push({ level: "error", message, meta }),
    },
  };
}

function settle<T>(promise: Promise<T>): Promise<{ value?: T; error?: unknown }> {
  return promise.then(
    (value) => ({ value }),
    (error: unknown) => ({ error }),
  );
}

/** Poll real time (not the bridge clock) until the fake engine has logged `tool`. */
async function waitForLogged(tool: string, generation: number): Promise<void> {
  const giveUpAt = Date.now() + 5_000;
  while (!loggedRequests().some((r) => r.tool === tool && r.generation === generation)) {
    if (Date.now() > giveUpAt) throw new Error(`engine never received ${tool} (gen ${generation})`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

function bridgeWithClock(clock: ManualClock, logger?: Logger): BinaryBridge {
  return new BinaryBridge(cachedExecutable(FAKE_ENGINE), workDir, {
    timeoutMs: 5_000,
    maxRestarts: 0,
    hangWindowMinMs: WINDOW_MS,
    hangProbeTimeoutMs: PROBE_MS,
    clock,
    ...(logger ? { logger } : {}),
  });
}

/** Pending request count, read from the bridge's internals. */
function pendingCount(bridge: BinaryBridge): number {
  return (bridge as unknown as { pending: Map<string, unknown> }).pending.size;
}

/** Whether the hang watchdog's liveness probe is waiting for its answer. */
function probeInFlight(bridge: BinaryBridge): boolean {
  return (bridge as unknown as { hangProbe: unknown }).hangProbe !== null;
}

/** Number of `ping` probes the fake engine of `generation` has received. */
function pingsLogged(generation: number): number {
  return loggedRequests().filter((r) => r.tool === "ping" && r.generation === generation).length;
}

/**
 * Poll real time (not the bridge clock) until the bridge has handled the
 * answer to its liveness probe.
 */
async function waitForProbeAnswered(bridge: BinaryBridge): Promise<void> {
  const giveUpAt = Date.now() + 5_000;
  while (probeInFlight(bridge)) {
    if (Date.now() > giveUpAt) throw new Error("liveness probe never answered");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

/**
 * Wedge the generation-1 engine with a call that times out on its own, then,
 * while the engine stays silent, start `tool` from another session so that it
 * is still waiting when the silence reaches the window and the liveness probe
 * goes unanswered. Time moves only through the manual clock, so every step
 * below happens at an exact instant.
 *
 * Timeline from the wedge (WINDOW_MS = 1000 ms, PROBE_MS = 100 ms): the wedge
 * times out at 100 ms; `tool` is sent at 700 ms with a 500 ms budget, so it
 * would wait until 1200 ms; the window stays 1000 ms (twice 500 ms). At
 * 999 ms nothing has happened; at 1000 ms the watchdog sends its `ping`, which
 * the wedged engine never answers; at 1100 ms the probe times out and the
 * child is killed while `tool` is still waiting.
 */
async function wedgeThen(
  bridge: BinaryBridge,
  clock: ManualClock,
  tool: string,
): Promise<{ value?: Record<string, unknown>; error?: unknown }> {
  const wedge = settle(bridge.toolCall("wedge-session", "wedge", {}, { timeoutMs: 100 }));
  clock.advance(100);
  expect(String(((await wedge).error as Error | undefined)?.message)).toContain("timed out");
  clock.advance(600);
  const call = settle(bridge.toolCall("other-session", tool, {}, { timeoutMs: 500 }));
  // The engine must have read the call before it is killed, so the request
  // log can show which generation saw it.
  await waitForLogged(tool, 1);
  clock.advance(299);
  expect(probeInFlight(bridge)).toBe(false);
  clock.advance(1);
  await waitForLogged("ping", 1);
  clock.advance(PROBE_MS - 1);
  expect(bridge.isAlive()).toBe(true);
  expect(pendingCount(bridge)).toBe(1);
  clock.advance(1);
  return call;
}

describe("request timeouts are isolated from other requests", () => {
  test("silent timeouts reject only themselves: siblings, a keep-warm shell call and the child survive", async () => {
    const { logger, logs } = recordingLogger();
    // Default 60 s watchdog floor: nothing here may be killed.
    const bridge = new BinaryBridge(cachedExecutable(FAKE_ENGINE), workDir, {
      timeoutMs: 5_000,
      maxRestarts: 0,
      logger,
    });
    const internals = bridge as unknown as { configured: boolean; process: { pid?: number } };

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const pid = internals.process.pid;

      const sibling = settle(
        bridge.toolCall("sibling-session", "read", { delay_ms: 400 }, { timeoutMs: 3_000 }),
      );
      const shell = settle(
        bridge.send(
          "bash",
          { command: "quiet build", delay_ms: 400, session_id: "shell-session" },
          { transportTimeoutMs: 3_000, keepBridgeOnTimeout: true },
        ),
      );

      // Two silent timeouts in a row from one session used to cross the
      // bridge-wide hang threshold and kill everything in flight.
      const timeouts: Array<{ error?: unknown }> = [];
      for (let i = 0; i < 2; i++) {
        timeouts.push(
          await settle(bridge.toolCall("timeout-session", "never", {}, { timeoutMs: 30 })),
        );
      }

      expect((await sibling).value).toMatchObject({ success: true, tool: "read", generation: 1 });
      expect((await shell).value).toMatchObject({ success: true, tool: "bash", generation: 1 });
      expect(bridge.isAlive()).toBe(true);
      expect(internals.process.pid).toBe(pid);
      expect(internals.configured).toBe(true);
      expect(logs.some(({ message }) => message.includes("Bridge killed"))).toBe(false);

      for (const timedOut of timeouts) {
        const message = String((timedOut.error as Error | undefined)?.message);
        expect(message).toContain("timed out after 30ms");
        expect(message).toContain("check its result before repeating it");
        expect((timedOut.error as Error).name).not.toBe("BridgeRestartedError");
      }
    } finally {
      await bridge.shutdown();
    }
  });
});

describe("hang watchdog", () => {
  for (const mutating of ["edit", "bash"] as const) {
    test(`a silent child with a waiting call is killed at the window; ${mutating} gets the typed error and is not retried`, async () => {
      const { logger, logs } = recordingLogger();
      const clock = new ManualClock();
      const bridge = bridgeWithClock(clock, logger);
      try {
        await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
        const call = await wedgeThen(bridge, clock, mutating);

        const error = call.error as
          | (Error & { retryable?: boolean; outcomeUnknown?: boolean; tool?: string })
          | undefined;
        expect(error?.name).toBe("BridgeRestartedError");
        expect(error?.retryable).toBe(true);
        expect(error?.outcomeUnknown).toBe(true);
        expect(error?.tool).toBe(mutating);
        expect(error?.message).toContain("inspect before repeating it");
        expect(bridge.isAlive()).toBe(false);

        // The call reached only the killed child; nothing re-sent it.
        expect(loggedRequests().filter(({ tool }) => tool === mutating)).toEqual([
          expect.objectContaining({ generation: 1 }),
        ]);
        expect(readFileSync(join(workDir, "spawns"), "utf8").trim().split("\n")).toHaveLength(1);

        const killLog = logs.find(
          ({ level, message }) =>
            level === "error" &&
            message.includes("Bridge killed after timeout of the hang watchdog"),
        );
        expect(killLog?.message).toContain(`silent_ms=${WINDOW_MS + PROBE_MS}`);
        expect(killLog?.message).toContain(`hang_window_ms=${WINDOW_MS}`);
        expect(killLog?.message).toContain(`probe_timeout_ms=${PROBE_MS}`);
        expect(killLog?.message).toContain('oldest_unanswered_tool="wedge"');
        expect(killLog?.message).toContain(`"tool":"${mutating}","retry":false`);
        expect(killLog?.meta).toEqual({ sessionId: "wedge-session" });
      } finally {
        await bridge.shutdown();
      }
    });
  }
  test("a read cut off by the watchdog is retried once and succeeds on the respawned child", async () => {
    const clock = new ManualClock();
    const bridge = bridgeWithClock(clock);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const read = await wedgeThen(bridge, clock, "read");

      expect(read.error).toBeUndefined();
      expect(read.value).toMatchObject({ success: true, tool: "read", generation: 2 });
      expect(bridge.isAlive()).toBe(true);
      expect(
        loggedRequests()
          .filter(({ tool }) => tool === "read")
          .map(({ generation }) => generation),
      ).toEqual([1, 2]);
    } finally {
      await bridge.shutdown();
    }
  });

  test("a child that keeps answering other calls is never killed, even with an unanswered call waiting past the window", async () => {
    const clock = new ManualClock();
    const bridge = bridgeWithClock(clock);
    const internals = bridge as unknown as { process: { pid?: number } };
    // Another session's call, answered by the engine at the current instant.
    const answeredCall = async (): Promise<void> => {
      expect(await bridge.toolCall("busy-session", "grep", {}, { timeoutMs: 1_000 })).toMatchObject(
        { success: true, tool: "grep" },
      );
    };
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const pid = internals.process.pid;

      // The same timeline as `wedgeThen`, except that the engine keeps
      // answering other calls (at 300, 650 and 900 ms). Without those
      // answers, `second` would still be waiting at 1000 ms, when the
      // child would have been silent for the whole window.
      const first = settle(bridge.toolCall("stuck-session", "never", {}, { timeoutMs: 100 }));
      clock.advance(100);
      expect(String(((await first).error as Error | undefined)?.message)).toContain("timed out");
      clock.advance(200);
      await answeredCall();
      clock.advance(350);
      await answeredCall();
      clock.advance(50);
      const second = settle(bridge.toolCall("stuck-session", "never", {}, { timeoutMs: 500 }));
      clock.advance(200);
      await answeredCall();
      clock.advance(100);
      expect(bridge.isAlive()).toBe(true);
      expect(pendingCount(bridge)).toBe(1);
      clock.advance(200);
      const secondError = (await second).error as Error | undefined;
      expect(String(secondError?.message)).toContain("timed out after 500ms");
      expect(secondError?.name).not.toBe("BridgeRestartedError");

      expect(bridge.isAlive()).toBe(true);
      expect(internals.process.pid).toBe(pid);
    } finally {
      await bridge.shutdown();
    }
  });
});

/**
 * With the default budgets (30 s per call, 60 s window, 5 s probe), a child
 * that goes completely silent while owing a reply is replaced one window plus
 * one probe timeout after its last output, even when every caller has already
 * timed out. A child that answers the probe is kept.
 */
describe("hang watchdog with default budgets", () => {
  function defaultBridge(clock: ManualClock, logger?: Logger): BinaryBridge {
    return new BinaryBridge(cachedExecutable(FAKE_ENGINE), workDir, {
      timeoutMs: 30_000,
      maxRestarts: 0,
      clock,
      ...(logger ? { logger } : {}),
    });
  }

  test("a silent child whose only caller timed out is replaced at 65 s (window plus unanswered probe), and a read retried at 75 s succeeds", async () => {
    const clock = new ManualClock();
    const bridge = defaultBridge(clock);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const first = settle(bridge.toolCall("s", "wedge", {}));
      await waitForLogged("wedge", 1);
      clock.advance(30_000);
      expect(String(((await first).error as Error | undefined)?.message)).toContain(
        "timed out after 30000ms",
      );
      // Nobody is waiting from here on; the child still owes the reply.
      clock.advance(29_999);
      expect(probeInFlight(bridge)).toBe(false);
      clock.advance(1);
      // At 60 s the watchdog probes; the wedged child never answers.
      await waitForLogged("ping", 1);
      clock.advance(4_999);
      expect(bridge.isAlive()).toBe(true);
      clock.advance(1);
      expect(bridge.isAlive()).toBe(false);

      clock.advance(10_000);
      const retry = await settle(bridge.toolCall("s", "read", {}));
      expect(retry.error).toBeUndefined();
      expect(retry.value).toMatchObject({ success: true, tool: "read", generation: 2 });
      expect(bridge.isAlive()).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("a read retried inside the window waits only until the window and the probe end, then succeeds on a fresh child", async () => {
    const clock = new ManualClock();
    const bridge = defaultBridge(clock);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const first = settle(bridge.toolCall("s", "wedge", {}));
      await waitForLogged("wedge", 1);
      clock.advance(30_000);
      await first;
      clock.advance(20_000);
      const retry = settle(bridge.toolCall("s", "read", {}));
      await waitForLogged("read", 1);
      clock.advance(10_000);
      await waitForLogged("ping", 1);
      clock.advance(4_999);
      expect(pendingCount(bridge)).toBe(1);
      clock.advance(1);
      const result = await retry;
      expect(result.error).toBeUndefined();
      expect(result.value).toMatchObject({ success: true, tool: "read", generation: 2 });
    } finally {
      await bridge.shutdown();
    }
  });

  test("a child that answers something after the unanswered call is not killed", async () => {
    const clock = new ManualClock();
    const bridge = defaultBridge(clock);
    const internals = bridge as unknown as { process: { pid?: number } };
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const pid = internals.process.pid;
      const first = settle(bridge.toolCall("s", "never", {}));
      clock.advance(30_000);
      await first;
      clock.advance(10_000);
      // Another call is answered at 40 s: the child is alive.
      expect(await bridge.toolCall("other", "grep", {})).toMatchObject({ success: true });
      // At 100 s the watchdog probes; the engine's answer arrives in real
      // time, so the clock waits for it before moving past the probe timeout.
      clock.advance(60_000);
      await waitForProbeAnswered(bridge);
      clock.advance(140_000);
      expect(bridge.isAlive()).toBe(true);
      expect(internals.process.pid).toBe(pid);
      expect(await bridge.toolCall("s", "read", {})).toMatchObject({
        success: true,
        generation: 1,
      });
    } finally {
      await bridge.shutdown();
    }
  });

  test("a child that answered one session and then froze on another's request is replaced one window plus the probe timeout after its last output, with no new request", async () => {
    const clock = new ManualClock();
    const bridge = defaultBridge(clock);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      // Session A's read is answered only when the test releases it; session
      // B's call, sent right after it, freezes the engine.
      const a = settle(bridge.toolCall("A", "read", { until_file: "release-a" }));
      await waitForLogged("read", 1);
      const b = settle(bridge.toolCall("B", "wedge", {}));
      await waitForLogged("wedge", 1);
      clock.advance(20_000);
      // A's reply is the child's last output, at 20 s.
      writeFileSync(join(workDir, "release-a"), "");
      expect((await a).value).toMatchObject({ success: true, tool: "read", generation: 1 });
      clock.advance(10_000);
      expect(String(((await b).error as Error | undefined)?.message)).toContain(
        "timed out after 30000ms",
      );

      // Nobody is waiting; B's request is still unanswered. 80 s is one
      // window after A's reply.
      clock.advance(49_999);
      expect(pingsLogged(1)).toBe(0);
      clock.advance(1);
      await waitForLogged("ping", 1);
      clock.advance(4_999);
      expect(bridge.isAlive()).toBe(true);
      clock.advance(1);
      expect(bridge.isAlive()).toBe(false);

      const next = await settle(bridge.toolCall("A", "read", {}));
      expect(next.value).toMatchObject({ success: true, tool: "read", generation: 2 });
    } finally {
      await bridge.shutdown();
    }
  });

  test("a child that keeps answering one session while another's request stays unanswered is never probed; once quiet, its answered probe keeps it", async () => {
    const { logger, logs } = recordingLogger();
    const clock = new ManualClock();
    const bridge = defaultBridge(clock, logger);
    const internals = bridge as unknown as { process: { pid?: number } };
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const pid = internals.process.pid;
      // B's request is never answered; its caller times out at 30 s.
      const b = settle(bridge.toolCall("B", "never", {}));
      await waitForLogged("never", 1);
      // A gets an answer every 5 s for 200 s, far past one window.
      for (let i = 0; i < 40; i++) {
        clock.advance(5_000);
        expect(await bridge.toolCall("A", "grep", {})).toMatchObject({ success: true });
      }
      expect(String(((await b).error as Error | undefined)?.message)).toContain(
        "timed out after 30000ms",
      );
      expect(pingsLogged(1)).toBe(0);
      expect(bridge.isAlive()).toBe(true);
      expect(internals.process.pid).toBe(pid);

      // A stops at 200 s. One window later the watchdog probes, the engine
      // answers, and B's abandoned request stops counting against the child.
      clock.advance(59_999);
      expect(pingsLogged(1)).toBe(0);
      clock.advance(1);
      await waitForProbeAnswered(bridge);
      expect(pingsLogged(1)).toBe(1);
      expect(
        logs.some(({ message }) =>
          message.includes("answered; child kept; dropped 1 unanswered request(s)"),
        ),
      ).toBe(true);
      clock.advance(200_000);
      expect(pingsLogged(1)).toBe(1);
      expect(bridge.isAlive()).toBe(true);
      expect(internals.process.pid).toBe(pid);
    } finally {
      await bridge.shutdown();
    }
  });

  test("a probe answered after its timeout does not undo the kill", async () => {
    const { logger, logs } = recordingLogger();
    const clock = new ManualClock();
    const bridge = defaultBridge(clock, logger);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      writeFileSync(join(workDir, "hold-pongs"), "");
      const first = settle(bridge.toolCall("s", "never", {}));
      await waitForLogged("never", 1);
      clock.advance(30_000);
      await first;
      clock.advance(30_000);
      await waitForLogged("ping", 1);

      // Let the engine write its answer into the pipe while this thread is
      // blocked, so the bridge cannot read it before the probe times out.
      writeFileSync(join(workDir, "release-pong"), "");
      const blocker = new Int32Array(new SharedArrayBuffer(4));
      const giveUpAt = Date.now() + 5_000;
      while (!existsSync(join(workDir, "pong-written"))) {
        if (Date.now() > giveUpAt) throw new Error("engine never wrote its probe answer");
        Atomics.wait(blocker, 0, 0, 5);
      }
      clock.advance(5_000);
      expect(bridge.isAlive()).toBe(false);

      // The answer, already in the pipe, is now delivered and must change nothing.
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(bridge.isAlive()).toBe(false);
      expect(logs.some(({ message }) => message.includes("Bridge killed after timeout"))).toBe(
        true,
      );
      expect(logs.some(({ message }) => message.includes("child kept"))).toBe(false);
      expect(readFileSync(join(workDir, "spawns"), "utf8").trim().split("\n")).toHaveLength(1);

      const next = await settle(bridge.toolCall("s", "read", {}));
      expect(next.value).toMatchObject({ success: true, tool: "read", generation: 2 });
    } finally {
      await bridge.shutdown();
    }
  });
});

describe("pooled bridges", () => {
  test("a pool passes its clock, hang window and probe timeout to the bridges it creates", async () => {
    const clock = new ManualClock();
    const pool = new BridgePool(cachedExecutable(FAKE_ENGINE), {
      idleTimeoutMs: Number.POSITIVE_INFINITY,
      timeoutMs: 5_000,
      maxRestarts: 0,
      hangWindowMinMs: WINDOW_MS,
      hangProbeTimeoutMs: PROBE_MS,
      clock,
    });
    try {
      const bridge = pool.getBridge(workDir);
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      // Fires at exactly WINDOW_MS + PROBE_MS on the pool's manual clock (see
      // wedgeThen); with the default 60 s window, the default 5 s probe or the
      // system clock it would not.
      const edit = await wedgeThen(bridge, clock, "edit");
      expect((edit.error as Error | undefined)?.name).toBe("BridgeRestartedError");
      expect(bridge.isAlive()).toBe(false);
    } finally {
      await pool.shutdown();
    }
  });
});
