/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { type ChildProcess, spawn } from "node:child_process";
import { constants } from "node:fs";
import { access, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { prepareSubcLane, recordSubcDaemonOwner, sweepReparentedSubcDaemons } from "./subc-rig.js";

const RIG_MODULE = resolve(import.meta.dir, "subc-rig.ts");
const POSIX_ONLY = process.platform === "win32";

const initialPrepared = await prepareSubcLane();
const exitSkipReason = POSIX_ONLY
  ? "the reparenting/kill-0 checks are POSIX-only"
  : initialPrepared.skipReason;

describe.skipIf(Boolean(exitSkipReason))(
  exitSkipReason
    ? `subc rig daemon does not outlive its runner (skipped: ${exitSkipReason})`
    : "subc rig daemon does not outlive its runner",
  () => {
    test("a runner that exits without cleanup still takes its daemon down", async () => {
      const scratch = await mkdtemp(join(tmpdir(), "subc-rig-exit-"));
      const scriptPath = join(scratch, "start-then-exit.ts");
      const resultPath = join(scratch, "started.json");
      // A stand-in for a test runner: start the rig, then end the process with
      // an explicit exit and no cleanup await, which is what `bun test` does.
      await writeFile(
        scriptPath,
        [
          `import { writeFileSync } from "node:fs";`,
          `import { prepareSubcLane, startSubcRig } from ${JSON.stringify(RIG_MODULE)};`,
          ``,
          `const prepared = await prepareSubcLane();`,
          `const rig = await startSubcRig(prepared);`,
          `writeFileSync(process.argv[2], JSON.stringify({ pid: rig.daemonPid, tempDir: rig.tempDir }), "utf8");`,
          `process.exit(0);`,
          ``,
        ].join("\n"),
        "utf8",
      );

      let daemonPid: number | undefined;
      let daemonTempDir: string | undefined;
      try {
        const child = spawn(process.execPath, [scriptPath, resultPath], {
          stdio: ["ignore", "pipe", "pipe"],
          env: {
            ...process.env,
            // Skip the child's cargo build; this process already resolved a binary.
            AFT_BINARY_PATH: initialPrepared.aftBinaryPath ?? "",
          },
        });
        const childOutput = collectOutput(child);
        const exitCode = await new Promise<number | null>((resolveExit) =>
          child.once("close", (code) => resolveExit(code)),
        );
        expect(exitCode, `child runner output:\n${childOutput()}`).toBe(0);

        const started = JSON.parse(await readFile(resultPath, "utf8")) as {
          pid?: number;
          tempDir?: string;
        };
        daemonPid = started.pid;
        daemonTempDir = started.tempDir;
        expect(typeof daemonPid).toBe("number");

        const pid = daemonPid as number;
        const died = await waitForDeath(pid, 2_000);
        expect(
          died,
          `daemon pid ${pid} survived its runner; the process exit handler did not fire`,
        ).toBe(true);
      } finally {
        if (daemonPid !== undefined && isAlive(daemonPid)) {
          // Only reached when the assertion above already failed; do not leave
          // the leaked daemon running for the next test file to inherit.
          try {
            process.kill(daemonPid, "SIGKILL");
          } catch {
            // already gone
          }
        }
        if (daemonTempDir) await rm(daemonTempDir, { recursive: true, force: true });
        await rm(scratch, { recursive: true, force: true });
      }
    }, 90_000);
  },
);

describe.skipIf(POSIX_ONLY)("subc rig orphan daemon sweep", () => {
  for (const scenario of [
    {
      name: "reaps an orphan with a dead recorded runner",
      orphan: true,
      owner: "dead",
      reaped: true,
    },
    {
      name: "leaves a daemon with a live recorded runner alone",
      orphan: false,
      owner: "live",
      reaped: false,
    },
    {
      name: "reaps a daemon with a dead recorded runner despite a non-1 live parent",
      orphan: false,
      owner: "dead",
      reaped: true,
    },
    {
      name: "leaves an unrelated unrecorded cache-root process with a live parent alone",
      orphan: false,
      owner: "none",
      reaped: false,
    },
    {
      name: "reaps a daemon when the recorded runner pid has been reused",
      orphan: false,
      owner: "reused",
      reaped: true,
    },
    { name: "leaves a reused daemon pid alone", orphan: false, owner: "stale", reaped: false },
  ]) {
    test(scenario.name, async () => {
      const sleepBinary = await resolveSleepBinary();
      const cacheRoot = await mkdtemp(join(tmpdir(), "subc-rig-sweep-cache-"));
      const fakeDaemon = join(cacheRoot, "ckdev-subc");
      expect(fakeDaemon.split(/[\\/]/).at(-1)).toBe("ckdev-subc");
      // Execute a symlink so macOS preserves the platform binary's signature.
      await symlink(sleepBinary, fakeDaemon);
      let planted: PlantedProcess | undefined;
      let runner: ChildProcess | undefined;
      try {
        planted = await plantProcess(fakeDaemon, { keepParentAlive: !scenario.orphan });
        if (scenario.owner !== "none") {
          runner = spawn(sleepBinary, ["300"], { stdio: "ignore" });
          const recordPath = await recordSubcDaemonOwner(planted.pid, cacheRoot, runner.pid!);
          if (scenario.owner === "reused" || scenario.owner === "stale") {
            const owner = JSON.parse(await readFile(recordPath, "utf8"));
            owner[scenario.owner === "reused" ? "runnerStart" : "daemonStart"] = "different-start";
            await writeFile(recordPath, JSON.stringify(owner));
          }
          if (scenario.owner === "dead" || scenario.owner === "stale") {
            const exited = new Promise<void>((done) => runner!.once("exit", () => done()));
            runner.kill("SIGKILL");
            await exited;
            expect(isAlive(runner.pid!), "recorded runner must be gone before sweeping").toBe(
              false,
            );
          }
        }
        const lines: string[] = [];
        const reaped = await sweepReparentedSubcDaemons({
          cacheRoot,
          log: (line) => lines.push(line),
        });
        expect(
          reaped.map((entry) => entry.pid),
          `sweep log:\n${lines.join("\n")}`,
        ).toEqual(scenario.reaped ? [planted.pid] : []);
        if (scenario.reaped) {
          expect(reaped[0]?.executable).toBe(fakeDaemon);
          expect(reaped[0]?.uptime).toMatch(/\d/);
          expect(await waitForDeath(planted.pid, 2_000)).toBe(true);
        } else {
          expect(isAlive(planted.pid), "protected process must survive the sweep").toBe(true);
        }
      } finally {
        planted?.dispose();
        runner?.kill("SIGKILL");
        await rm(cacheRoot, { recursive: true, force: true });
      }
    }, 30_000);
  }
});

describe("subc rig orphan daemon sweep on win32", () => {
  test("no-ops with a log line instead of reading a process table", async () => {
    const realPlatform = process.platform;
    Object.defineProperty(process, "platform", { value: "win32", configurable: true });
    try {
      const lines: string[] = [];
      const reaped = await sweepReparentedSubcDaemons({
        cacheRoot: join(tmpdir(), "subc-rig-sweep-win32-absent"),
        log: (line) => lines.push(line),
      });
      expect(reaped).toEqual([]);
      expect(lines.join("\n")).toContain("win32");
    } finally {
      Object.defineProperty(process, "platform", { value: realPlatform, configurable: true });
    }
  });
});

interface PlantedProcess {
  pid: number;
  dispose(): void;
}

/**
 * Start `exe` from a shell so the test controls whether its parent stays alive.
 * With `keepParentAlive: false` the shell exits immediately and the kernel
 * reparents the child to pid 1 or a child subreaper, as with a killed runner.
 */
async function plantProcess(
  exe: string,
  options: { keepParentAlive: boolean },
): Promise<PlantedProcess> {
  const script = options.keepParentAlive
    ? `"${exe}" 300 & echo $!; wait`
    : `"${exe}" 300 & echo $!`;
  const shell = spawn("sh", ["-c", script], { stdio: ["ignore", "pipe", "pipe"] });
  const pid = await firstPidLine(shell);
  const ready = await waitUntil(() => {
    const parent = parentOf(pid);
    // The shell can print $! and exit before its child execs. Reparenting alone
    // is not readiness: the sweep ignores commands outside the daemon cache.
    return (
      isAlive(pid) &&
      commandOf(pid).startsWith(`${exe} `) &&
      parent !== null &&
      (options.keepParentAlive
        ? parent === shell.pid && parent !== 1
        : parent !== shell.pid && shell.exitCode !== null)
    );
  }, 2_000);
  if (!ready) {
    const parent = parentOf(pid);
    const parentCommand = parent === null ? "<missing>" : commandOf(parent);
    const childCommand = commandOf(pid);
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      /* already gone */
    }
    shell.kill("SIGKILL");
    throw new Error(
      `Planting pid ${pid} failed: expected ${exe} with ${options.keepParentAlive ? "live shell parent" : "reparented child"}; actual command=${childCommand}, parent pid=${parent}, command=${parentCommand}`,
    );
  }
  return {
    pid,
    dispose: () => {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // already reaped
      }
      shell.kill("SIGKILL");
    },
  };
}

async function firstPidLine(child: ChildProcess): Promise<number> {
  return new Promise<number>((resolvePid, rejectPid) => {
    let buffer = "";
    const timer = setTimeout(() => rejectPid(new Error("planted process printed no pid")), 5_000);
    child.stdout?.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      const line = buffer.split("\n")[0]?.trim();
      if (!line) return;
      const pid = Number(line);
      if (!Number.isFinite(pid)) {
        clearTimeout(timer);
        rejectPid(new Error(`planted process printed a non-numeric pid: ${line}`));
        return;
      }
      clearTimeout(timer);
      resolvePid(pid);
    });
  });
}

function collectOutput(child: ChildProcess): () => string {
  let text = "";
  child.stdout?.on("data", (chunk: Buffer) => {
    text += chunk.toString("utf8");
  });
  child.stderr?.on("data", (chunk: Buffer) => {
    text += chunk.toString("utf8");
  });
  return () => text.trim();
}

function parentOf(pid: number): number | null {
  const result = Bun.spawnSync(["ps", "-o", "ppid=", "-p", String(pid)]);
  const text = new TextDecoder().decode(result.stdout).trim();
  const ppid = Number(text);
  return Number.isFinite(ppid) && text.length > 0 ? ppid : null;
}

function commandOf(pid: number): string {
  const result = Bun.spawnSync(["ps", "-p", String(pid), "-o", "command="]);
  return new TextDecoder().decode(result.stdout).trim();
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitForDeath(pid: number, timeoutMs: number): Promise<boolean> {
  return waitUntil(() => !isAlive(pid), timeoutMs);
}

async function waitUntil(predicate: () => boolean, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((resolveSleep) => setTimeout(resolveSleep, 25));
  }
  return predicate();
}

async function resolveSleepBinary(): Promise<string> {
  for (const candidate of ["/bin/sleep", "/usr/bin/sleep"]) {
    try {
      await access(candidate, constants.X_OK);
      return candidate;
    } catch {
      // try the next location
    }
  }
  throw new Error("no sleep binary found for the sweep fixture");
}
