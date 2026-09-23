/// <reference path="../bun-test.d.ts" />

/**
 * SIGTERM must end an OpenCode host running the plugin, even when another
 * SIGTERM listener is present and never exits. Registering any signal listener
 * turns off the runtime's default terminate-on-signal, so AFT's handler has to
 * guarantee the exit once its bounded cleanup window has passed.
 */

import { describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { resolve } from "node:path";

/** The plugin's 5 s bound plus slack for process startup and teardown. */
const EXIT_BOUND_MS = 8_000;
const HANG_KILL_MS = 15_000;

describe.skipIf(process.platform === "win32")("OpenCode shutdown hooks on SIGTERM", () => {
  test("exits with 143 within the bound even when another SIGTERM listener never exits", async () => {
    const child = spawn(
      process.execPath,
      ["run", resolve(import.meta.dir, "fixtures/shutdown-signal-harness.ts")],
      { stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, AFT_LOG_STDERR: "1" } },
    );
    const events: string[] = [];
    let signalledAt: number | null = null;
    let killedAsHung = false;
    let hangTimer: ReturnType<typeof setTimeout> | undefined;
    let stderr = "";
    child.stderr.on("data", (chunk) => {
      stderr = (stderr + String(chunk)).slice(-4_000);
    });
    let stdoutBuf = "";
    child.stdout.on("data", (chunk) => {
      stdoutBuf += String(chunk);
      let newline = stdoutBuf.indexOf("\n");
      while (newline >= 0) {
        const line = stdoutBuf.slice(0, newline);
        stdoutBuf = stdoutBuf.slice(newline + 1);
        newline = stdoutBuf.indexOf("\n");
        const name = /^EVENT (\S+)/.exec(line)?.[1];
        if (!name) continue;
        events.push(name);
        if (name === "awaiting-signal") {
          signalledAt = Date.now();
          child.kill("SIGTERM");
          hangTimer = setTimeout(() => {
            killedAsHung = true;
            child.kill("SIGKILL");
          }, HANG_KILL_MS);
        }
      }
    });
    const startupTimer = setTimeout(() => {
      killedAsHung = true;
      child.kill("SIGKILL");
    }, 30_000);
    const exit = await new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(
      (resolveExit) => {
        child.on("exit", (code, signal) => resolveExit({ code, signal }));
      },
    );
    clearTimeout(startupTimer);
    clearTimeout(hangTimer);
    const elapsed = signalledAt === null ? Number.POSITIVE_INFINITY : Date.now() - signalledAt;

    if (killedAsHung) console.error(`harness stderr tail:\n${stderr}`);
    expect(events).toContain("awaiting-signal");
    expect(events).toContain("aft-cleanup-ran");
    expect(events).toContain("host-listener-called");
    expect(killedAsHung).toBe(false);
    expect(exit).toEqual({ code: 143, signal: null });
    expect(elapsed).toBeLessThan(EXIT_BOUND_MS);
  }, 60_000);
});
