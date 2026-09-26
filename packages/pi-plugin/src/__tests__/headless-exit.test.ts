/// <reference path="../bun-test.d.ts" />

/**
 * A headless `pi -p` run must exit once Pi has fired `session_shutdown`.
 *
 * GitHub issue #336: with aft-pi loaded, `pi -p` printed its answer within a
 * few seconds but the process lived on for another minute (and forever with a
 * second extension installed). These tests run the real extension in a child
 * process (see fixtures/headless-exit-harness.ts), shut the session down while
 * an LSP auto-install is still running, and watch what the process does next.
 * A second suite sends SIGTERM to a host that has another SIGTERM listener
 * which never exits, and checks the plugin still ends the process.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";

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

const tempDirs: string[] = [];
let runPromise: Promise<HarnessRun> | undefined;

/** Build a throwaway HOME/XDG/project sandbox and start the harness in it. */
function spawnHarness(mode: "session-shutdown" | "sigterm" | "subagent") {
  const tempDir = mkdtempSync(join(tmpdir(), "aft-pi-headless-exit-"));
  tempDirs.push(tempDir);
  const binDir = join(tempDir, "bin");
  const projectDir = join(tempDir, "project");
  const configDir = join(tempDir, "config");
  const npmMarker = join(tempDir, "npm.pid");
  mkdirSync(binDir, { recursive: true });
  mkdirSync(projectDir, { recursive: true });
  mkdirSync(join(configDir, "cortexkit"), { recursive: true });
  // A stand-in for npm that records its pid and then just sits there, so the
  // LSP install is guaranteed to still be running when the session ends.
  linkCachedExecutable(
    join(binDir, "npm"),
    '#!/bin/sh\necho $$ > "$HARNESS_NPM_MARKER"\nexec sleep 60\n',
  );
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
    HARNESS_MODE: mode,
  });
  if (mode === "subagent") env.MAGIC_CONTEXT_PI_SUBAGENT = "1";
  else delete env.MAGIC_CONTEXT_PI_SUBAGENT;

  const child = spawn(
    process.execPath,
    ["run", resolve(import.meta.dir, "fixtures/headless-exit-harness.ts")],
    { cwd: projectDir, env, stdio: ["ignore", "pipe", "pipe"] },
  );
  return { child, npmMarker };
}

function readNpmPid(npmMarker: string): number | null {
  try {
    return Number.parseInt(readFileSync(npmMarker, "utf8").trim(), 10);
  } catch {
    return null;
  }
}

function runHarness(): Promise<HarnessRun> {
  runPromise ??= new Promise<HarnessRun>((resolveRun, rejectRun) => {
    const { child, npmMarker } = spawnHarness("session-shutdown");
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
      run.npmPid = readNpmPid(npmMarker);
      resolveRun(run);
    });
  });
  return runPromise;
}

interface SigtermRun {
  events: string[];
  exitCode: number | null;
  exitSignal: NodeJS.Signals | null;
  exitedAfterSignalMs: number | null;
  killedAsHung: boolean;
  npmPid: number | null;
  stderr: string;
}

/** Longest the process may live after SIGTERM: the plugin's bound plus slack. */
const SIGTERM_EXIT_BOUND_MS = 8_000;
let sigtermNpmPid: number | null = null;

function runSigtermHarness(): Promise<SigtermRun> {
  return new Promise<SigtermRun>((resolveRun, rejectRun) => {
    const { child, npmMarker } = spawnHarness("sigterm");
    const run: SigtermRun = {
      events: [],
      exitCode: null,
      exitSignal: null,
      exitedAfterSignalMs: null,
      killedAsHung: false,
      npmPid: null,
      stderr: "",
    };
    let signalledAt: number | null = null;
    let hangTimer: ReturnType<typeof setTimeout> | undefined;
    let stdoutBuf = "";
    child.stdout.on("data", (chunk) => {
      stdoutBuf += String(chunk);
      let newline = stdoutBuf.indexOf("\n");
      while (newline >= 0) {
        const line = stdoutBuf.slice(0, newline);
        stdoutBuf = stdoutBuf.slice(newline + 1);
        newline = stdoutBuf.indexOf("\n");
        const match = /^EVENT (\S+)/.exec(line);
        if (!match) continue;
        run.events.push(match[1] ?? "");
        if (match[1] === "awaiting-signal") {
          signalledAt = Date.now();
          child.kill("SIGTERM");
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
    }, 60_000);
    child.on("error", rejectRun);
    child.on("exit", (code, signal) => {
      clearTimeout(overallTimer);
      clearTimeout(hangTimer);
      run.exitCode = code;
      run.exitSignal = signal;
      if (signalledAt !== null && !run.killedAsHung) {
        run.exitedAfterSignalMs = Date.now() - signalledAt;
      }
      run.npmPid = readNpmPid(npmMarker);
      sigtermNpmPid = run.npmPid;
      resolveRun(run);
    });
  });
}

// Removing the sandboxes can be slow on a loaded machine (the full workspace
// suite runs packages in parallel), so allow more than the default hook time.
afterAll(async () => {
  const run = await runPromise?.catch(() => undefined);
  // Never leave the stand-in npm behind if a regression orphaned it.
  for (const pid of [run?.npmPid, sigtermNpmPid]) {
    if (pid && isAlive(pid)) process.kill(pid, "SIGKILL");
  }
  for (const dir of tempDirs) rmSync(dir, { recursive: true, force: true });
}, 30_000);

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

describe.skipIf(process.platform === "win32")("SIGTERM ends a Pi host running the plugin", () => {
  // Pi's signal-exit listener only re-raises when it is the only listener, so
  // with AFT's handler present nobody else exits the process: AFT must.
  test("exits with 143 within the bound even when another SIGTERM listener never exits", async () => {
    const run = await runSigtermHarness();
    expect(run.events).toContain("awaiting-signal");
    expect(run.events).toContain("host-listener-called");
    if (run.killedAsHung) console.error(`harness stderr tail:\n${run.stderr}`);
    expect(run.killedAsHung).toBe(false);
    expect({ code: run.exitCode, signal: run.exitSignal }).toEqual({ code: 143, signal: null });
    expect(run.exitedAfterSignalMs ?? Number.POSITIVE_INFINITY).toBeLessThan(SIGTERM_EXIT_BOUND_MS);
    // The plugin's own cleanup still ran: the in-flight npm install is gone.
    expect(run.npmPid).not.toBeNull();
    expect(isAlive(run.npmPid ?? -1)).toBe(false);
  }, 70_000);
});

interface SubagentRun {
  events: Array<{ name: string; detail: string }>;
  exitCode: number | null;
  killedAsHung: boolean;
  npmPid: number | null;
  stderr: string;
}

function runSubagentHarness(): Promise<SubagentRun> {
  return new Promise<SubagentRun>((resolveRun, rejectRun) => {
    const { child, npmMarker } = spawnHarness("subagent");
    const run: SubagentRun = {
      events: [],
      exitCode: null,
      killedAsHung: false,
      npmPid: null,
      stderr: "",
    };
    let stdoutBuf = "";
    child.stdout.on("data", (chunk) => {
      stdoutBuf += String(chunk);
      let newline = stdoutBuf.indexOf("\n");
      while (newline >= 0) {
        const line = stdoutBuf.slice(0, newline);
        stdoutBuf = stdoutBuf.slice(newline + 1);
        newline = stdoutBuf.indexOf("\n");
        const match = /^EVENT (\S+) ?(.*)$/.exec(line);
        if (match) run.events.push({ name: match[1] ?? "", detail: match[2] ?? "" });
      }
    });
    child.stderr.on("data", (chunk) => {
      run.stderr = (run.stderr + String(chunk)).slice(-4_000);
    });
    const overallTimer = setTimeout(() => {
      run.killedAsHung = true;
      child.kill("SIGKILL");
    }, 45_000);
    child.on("error", rejectRun);
    child.on("exit", (code) => {
      clearTimeout(overallTimer);
      run.exitCode = code;
      run.npmPid = readNpmPid(npmMarker);
      sigtermNpmPid ??= run.npmPid;
      resolveRun(run);
    });
  });
}

let subagentRunPromise: Promise<SubagentRun> | undefined;
const subagentRun = () => {
  subagentRunPromise ??= runSubagentHarness();
  return subagentRunPromise;
};
const eventsBefore = (run: SubagentRun, marker: string) => {
  const index = run.events.findIndex((event) => event.name === marker);
  return index < 0 ? run.events : run.events.slice(0, index);
};

describe.skipIf(process.platform === "win32")(
  "pi-magic-context subagent (MAGIC_CONTEXT_PI_SUBAGENT=1)",
  () => {
    test("startup runs no warmup, no ONNX Runtime preparation and no LSP install", async () => {
      const run = await subagentRun();
      if (run.killedAsHung || run.exitCode !== 0) console.error(`harness stderr:\n${run.stderr}`);
      expect(run.events.map((event) => event.name)).toContain("startup-quiet");
      const startup = eventsBefore(run, "startup-quiet").map((event) => event.name);
      expect(startup).not.toContain("bridge-spawn");
      expect(startup).not.toContain("onnx-prepare");
      expect(startup).toContain("npm-never-started");
      expect(run.npmPid).toBeNull();
    }, 60_000);

    test("an AFT tool call still reaches a lazily spawned bridge", async () => {
      const run = await subagentRun();
      const afterStartup = run.events.slice(
        run.events.findIndex((event) => event.name === "startup-quiet") + 1,
      );
      expect(afterStartup.map((event) => [event.name, event.detail.split(" ")[1] ?? ""])).toEqual(
        expect.arrayContaining([
          ["bridge-spawn", "tool=outline"],
          ["bridge-tool-call", ""],
        ]),
      );
      expect(run.events.find((event) => event.name === "bridge-tool-call")?.detail).toBe("outline");
      expect(run.events.find((event) => event.name === "tool-result")?.detail).toContain(
        "outline ok",
      );
      expect(run.killedAsHung).toBe(false);
      expect(run.exitCode).toBe(0);
    }, 60_000);
  },
);
