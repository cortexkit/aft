/// <reference path="../bun-test.d.ts" />

/**
 * A headless `pi -p` run must exit once Pi has fired `session_shutdown`.
 *
 * GitHub issue #336: with aft-pi loaded, `pi -p` printed its answer within a
 * few seconds but the process lived on for another minute (and forever with a
 * second extension installed). These tests run the real extension in a child
 * process (see fixtures/headless-exit-harness.ts), shut the session down while
 * an LSP auto-install is still running, and watch what the process does next.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

/** How long the process may live after its shutdown hook returned. */
const EXIT_BOUND_MS = 5_000;
/** When to stop waiting and kill a child that is evidently not going to exit. */
const HANG_KILL_MS = 15_000;

interface HarnessRun {
  events: Array<{ name: string; detail: string; at: number }>;
  exitedAfterShutdownMs: number | null;
  killedAsHung: boolean;
  npmPid: number | null;
  stderr: string;
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

let tempDir: string | undefined;
let runPromise: Promise<HarnessRun> | undefined;

function runHarness(): Promise<HarnessRun> {
  runPromise ??= new Promise<HarnessRun>((resolveRun, rejectRun) => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-pi-headless-exit-"));
    const binDir = join(tempDir, "bin");
    const projectDir = join(tempDir, "project");
    const configDir = join(tempDir, "config");
    const npmMarker = join(tempDir, "npm.pid");
    mkdirSync(binDir, { recursive: true });
    mkdirSync(projectDir, { recursive: true });
    mkdirSync(join(configDir, "cortexkit"), { recursive: true });
    // A stand-in for npm that records its pid and then just sits there, so the
    // LSP install is guaranteed to still be running when the session ends.
    writeFileSync(
      join(binDir, "npm"),
      '#!/bin/sh\necho $$ > "$HARNESS_NPM_MARKER"\nexec sleep 60\n',
    );
    chmodSync(join(binDir, "npm"), 0o755);
    // A TypeScript project makes typescript-language-server relevant; pinning
    // its version skips the registry probe so no network is needed.
    writeFileSync(join(projectDir, "a.ts"), "export const a = 1;\n");
    writeFileSync(join(projectDir, "package.json"), '{ "name": "headless-exit" }\n');
    writeFileSync(
      join(configDir, "cortexkit", "aft.jsonc"),
      '{ "semantic_search": true, "lsp": { "versions": { "typescript-language-server": "4.3.3" } } }\n',
    );

    const env: Record<string, string> = {};
    for (const [key, value] of Object.entries(process.env)) {
      if (value !== undefined) env[key] = value;
    }
    delete env.AFT_CACHE_DIR;
    Object.assign(env, {
      HOME: join(tempDir, "home"),
      XDG_CONFIG_HOME: configDir,
      XDG_CACHE_HOME: join(tempDir, "cache"),
      XDG_DATA_HOME: join(tempDir, "data"),
      XDG_STATE_HOME: join(tempDir, "state"),
      PATH: `${binDir}:/usr/bin:/bin`,
      HARNESS_NPM_MARKER: npmMarker,
      HARNESS_PLUGIN: resolve(import.meta.dir, "../index.ts"),
    });

    const child = spawn(
      process.execPath,
      ["run", resolve(import.meta.dir, "fixtures/headless-exit-harness.ts")],
      { cwd: projectDir, env, stdio: ["ignore", "pipe", "pipe"] },
    );
    const run: HarnessRun = {
      events: [],
      exitedAfterShutdownMs: null,
      killedAsHung: false,
      npmPid: null,
      stderr: "",
    };
    let shutdownAt: number | null = null;
    let hangTimer: ReturnType<typeof setTimeout> | undefined;
    let stdoutBuf = "";
    child.stdout.on("data", (chunk) => {
      stdoutBuf += String(chunk);
      let newline = stdoutBuf.indexOf("\n");
      while (newline >= 0) {
        const line = stdoutBuf.slice(0, newline);
        stdoutBuf = stdoutBuf.slice(newline + 1);
        newline = stdoutBuf.indexOf("\n");
        const match = /^EVENT (\S+) ?(.*)$/.exec(line);
        if (!match) continue;
        const event = { name: match[1] ?? "", detail: match[2] ?? "", at: Date.now() };
        run.events.push(event);
        if (event.name === "shutdown-done") {
          shutdownAt = event.at;
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
    // Covers a harness that never reaches shutdown at all.
    const overallTimer = setTimeout(() => {
      run.killedAsHung = true;
      child.kill("SIGKILL");
    }, 60_000);
    child.on("error", rejectRun);
    child.on("exit", () => {
      clearTimeout(overallTimer);
      clearTimeout(hangTimer);
      if (shutdownAt !== null && !run.killedAsHung) {
        run.exitedAfterShutdownMs = Date.now() - shutdownAt;
      }
      try {
        run.npmPid = Number.parseInt(readFileSync(npmMarker, "utf8").trim(), 10);
      } catch {
        run.npmPid = null;
      }
      resolveRun(run);
    });
  });
  return runPromise;
}

afterAll(async () => {
  const run = await runPromise?.catch(() => undefined);
  // Never leave the stand-in npm behind if a regression orphaned it.
  if (run?.npmPid && isAlive(run.npmPid)) process.kill(run.npmPid, "SIGKILL");
  if (tempDir) rmSync(tempDir, { recursive: true, force: true });
});

const eventsAfterShutdown = (run: HarnessRun) => {
  const index = run.events.findIndex((event) => event.name === "shutdown-done");
  return index < 0 ? [] : run.events.slice(index + 1);
};

describe.skipIf(process.platform === "win32")(
  "headless Pi run exits after session_shutdown",
  () => {
    test("the process exits promptly once session_shutdown returns with an LSP install in flight", async () => {
      const run = await runHarness();
      const names = run.events.map((event) => event.name);
      expect(names).toContain("npm-started");
      expect(names).toContain("shutdown-done");
      if (run.killedAsHung) console.error(`harness stderr tail:\n${run.stderr}`);
      expect(run.killedAsHung).toBe(false);
      expect(run.exitedAfterShutdownMs).not.toBeNull();
      expect(run.exitedAfterShutdownMs ?? Number.POSITIVE_INFINITY).toBeLessThan(EXIT_BOUND_MS);
    }, 70_000);

    test("nothing revives the bridge pool or spawns a bridge after session_shutdown", async () => {
      const run = await runHarness();
      const late = eventsAfterShutdown(run).filter(
        (event) => event.name === "pool-revived" || event.name === "bridge-spawn",
      );
      expect(late).toEqual([]);
    }, 70_000);

    test("the in-flight npm install is stopped with the session instead of left running", async () => {
      const run = await runHarness();
      expect(run.npmPid).not.toBeNull();
      expect(isAlive(run.npmPid ?? -1)).toBe(false);
    }, 70_000);
  },
);
