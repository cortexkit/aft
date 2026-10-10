/// <reference path="../bun-test.d.ts" />

/**
 * A warm bridge must not keep its host process alive once nothing is in
 * flight, and must keep it alive while a call is.
 *
 * One-shot host commands load the plugin and then simply return; OMP's
 * `plugin install` does exactly that (issue #389). Node and Bun exit when no
 * referenced handle is left, and a spawned child with open stdio pipes is
 * such a handle. So each case runs a real BinaryBridge with a real stub child
 * in its own process, and the assertion is whether that process exits.
 */

import { describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { join } from "node:path";
import { cachedExecutable } from "./test-utils/cached-executable.js";

const HARNESS = join(import.meta.dir, "fixtures", "bridge-loop-hold-harness.ts");
const BRIDGE_MODULE = join(import.meta.dir, "..", "bridge.ts");
// How long the harness may live after its call settled. An idle bridge lets it
// exit within a few hundred milliseconds; a held loop never exits at all.
const EXIT_BOUND_MS = 3_000;
const HANG_KILL_MS = 8_000;
// The stub's status frame arrives this long after configure, while the call
// is still waiting on the configure-warnings handler; a background task's
// completion frame arrives this long after the task started.
const STATUS_FRAME_DELAY_MS = 1_000;

// Stub `aft`: answers every request. In "busy-gap" mode it returns one
// configure warning and pushes a status frame a second later; a `bash` request
// starts a background task whose completion frame follows a second later. It
// exits when its stdin closes, as the real binary does.
const STUB_SOURCE = `#!/usr/bin/env node
const mode = process.env.HARNESS_MODE;
process.stdin.setEncoding("utf8");
let buffer = "";
const write = (frame) => process.stdout.write(JSON.stringify(frame) + "\\n");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const req = JSON.parse(buffer.slice(0, newline));
    buffer = buffer.slice(newline + 1);
    if (req.command === "configure") {
      const busy = mode === "busy-gap";
      write({ id: req.id, success: true, warnings: busy ? [{ code: "stub", message: "held" }] : [] });
      if (busy) {
        setTimeout(() => write({ type: "status_changed", snapshot: { stub: true } }), ${STATUS_FRAME_DELAY_MS});
      }
    } else if (req.command === "bash") {
      write({ id: req.id, success: true, task_id: "stub-task", status: "running" });
      setTimeout(() => write({
        type: "bash_completed",
        task_id: "stub-task",
        session_id: "stub-session",
        status: "completed",
        exit_code: 0,
        command: "sleep 1",
      }), ${STATUS_FRAME_DELAY_MS});
    } else {
      write({ id: req.id, success: true });
    }
  }
});
process.stdin.on("end", () => process.exit(0));
`;

interface HarnessEvent {
  name: string;
  detail: string;
  at: number;
}

interface HarnessRun {
  events: HarnessEvent[];
  exitCode: number | null;
  killedAsHung: boolean;
  exitedAfterSettledMs: number | null;
  stderr: string;
}

function runHarness(mode: "idle" | "busy-gap" | "background-task"): Promise<HarnessRun> {
  const binary = cachedExecutable(STUB_SOURCE);
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(process.execPath, ["run", HARNESS], {
      env: {
        ...process.env,
        HARNESS_BRIDGE: BRIDGE_MODULE,
        HARNESS_BINARY: binary,
        HARNESS_MODE: mode,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const run: HarnessRun = {
      events: [],
      exitCode: null,
      killedAsHung: false,
      exitedAfterSettledMs: null,
      stderr: "",
    };
    let settledAt: number | null = null;
    let hangTimer: ReturnType<typeof setTimeout> | undefined;
    let stdoutBuffer = "";
    child.stdout.on("data", (chunk) => {
      stdoutBuffer += String(chunk);
      let newline = stdoutBuffer.indexOf("\n");
      while (newline !== -1) {
        const line = stdoutBuffer.slice(0, newline);
        stdoutBuffer = stdoutBuffer.slice(newline + 1);
        newline = stdoutBuffer.indexOf("\n");
        const match = /^EVENT (\S+) ?(.*)$/.exec(line);
        if (!match) continue;
        const event = { name: match[1] ?? "", detail: match[2] ?? "", at: Date.now() };
        run.events.push(event);
        if (event.name === "call-settled") {
          settledAt = event.at;
          hangTimer = setTimeout(() => {
            run.killedAsHung = true;
            child.kill("SIGKILL");
          }, HANG_KILL_MS);
        }
      }
    });
    child.stderr.on("data", (chunk) => {
      run.stderr = (run.stderr + String(chunk)).slice(-4_000);
    });
    const overallTimer = setTimeout(() => {
      run.killedAsHung = true;
      child.kill("SIGKILL");
    }, 30_000);
    child.on("error", rejectRun);
    child.on("exit", (code) => {
      clearTimeout(overallTimer);
      clearTimeout(hangTimer);
      run.exitCode = code;
      if (settledAt !== null && !run.killedAsHung)
        run.exitedAfterSettledMs = Date.now() - settledAt;
      resolveRun(run);
    });
  });
}

describe.skipIf(process.platform === "win32")("BinaryBridge event-loop hold", () => {
  test("an idle bridge with a live child does not keep the host process alive", async () => {
    const run = await runHarness("idle");
    if (run.killedAsHung || run.exitCode !== 0) console.error(`harness stderr:\n${run.stderr}`);
    const settled = run.events.find((event) => event.name === "call-settled");
    // The child was still running when the script ended, so it is the bridge's
    // idle state, not a dead child, that lets the process exit.
    expect(settled?.detail).toBe("success=true alive=true");
    expect(run.killedAsHung).toBe(false);
    expect(run.exitCode).toBe(0);
    expect(run.exitedAfterSettledMs ?? Number.POSITIVE_INFINITY).toBeLessThan(EXIT_BOUND_MS);
  }, 45_000);

  test("a call in flight keeps the host process alive while no request timer is armed", async () => {
    const run = await runHarness("busy-gap");
    if (run.killedAsHung || run.exitCode !== 0) console.error(`harness stderr:\n${run.stderr}`);
    const names = run.events.map((event) => event.name);
    // Between configure's reply and the stub's status frame the call awaits the
    // configure-warnings handler: only the bridge's hold on the child keeps the
    // process running long enough to receive the frame and finish the call.
    expect(names).toEqual([
      "configure-warnings-waiting",
      "status-frame",
      "call-settled",
      "shutdown-done",
    ]);
    const waiting = run.events[0]?.at ?? 0;
    const frame = run.events[1]?.at ?? 0;
    expect(frame - waiting).toBeGreaterThanOrEqual(STATUS_FRAME_DELAY_MS - 200);
    expect(run.events[2]?.detail).toBe("success=true alive=true");
    expect(run.killedAsHung).toBe(false);
    expect(run.exitCode).toBe(0);
  }, 45_000);

  test("a background bash task keeps the host alive until its completion arrives, then releases it", async () => {
    const run = await runHarness("background-task");
    if (run.killedAsHung || run.exitCode !== 0) console.error(`harness stderr:\n${run.stderr}`);
    const names = run.events.map((event) => event.name);
    // The call itself returned at once; only the outstanding task held the
    // process until the completion frame, and nothing held it afterwards.
    expect(names).toEqual(["call-settled", "bash-completed"]);
    const settled = run.events[0]?.at ?? 0;
    const completed = run.events[1]?.at ?? 0;
    expect(completed - settled).toBeGreaterThanOrEqual(STATUS_FRAME_DELAY_MS - 200);
    expect(run.events[1]?.detail).toBe("task=stub-task");
    expect(run.killedAsHung).toBe(false);
    expect(run.exitCode).toBe(0);
    expect(
      (run.exitedAfterSettledMs ?? Number.POSITIVE_INFINITY) - (completed - settled),
    ).toBeLessThan(EXIT_BOUND_MS);
  }, 45_000);
});
