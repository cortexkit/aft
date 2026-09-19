/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { type ChildProcess, spawn } from "node:child_process";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { prepareSubcLane, type ReapedSubcDaemon, sweepReparentedSubcDaemons } from "./subc-rig.js";

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
  test("reaps a reparented cache-root process and leaves a live-parent one alone", async () => {
    const sleepBinary = await resolveSleepBinary();
    const cacheRoot = await mkdtemp(join(tmpdir(), "subc-rig-sweep-cache-"));
    const fakeDaemon = join(cacheRoot, "subc-core-vtest", "ck-subc");
    await mkdir(join(cacheRoot, "subc-core-vtest"), { recursive: true });
    // A symlink, not a copy: macOS kills copies of platform binaries, while
    // exec through a symlink reports the link path in `ps` (which is what the
    // sweep matches on) and runs the real binary.
    await symlink(sleepBinary, fakeDaemon);

    let orphan: PlantedProcess | undefined;
    let adopted: PlantedProcess | undefined;
    try {
      orphan = await plantProcess(fakeDaemon, { keepParentAlive: false });
      adopted = await plantProcess(fakeDaemon, { keepParentAlive: true });

      const lines: string[] = [];
      let reaped: ReapedSubcDaemon[] = [];
      reaped = await sweepReparentedSubcDaemons({
        cacheRoot,
        log: (line) => lines.push(line),
      });

      const orphanPid = orphan.pid;
      const adoptedPid = adopted.pid;
      expect(
        reaped.map((entry) => entry.pid),
        `sweep log:\n${lines.join("\n")}`,
      ).toEqual([orphanPid]);
      expect(reaped[0]?.executable).toBe(fakeDaemon);
      expect(reaped[0]?.uptime).toMatch(/\d/);
      expect(lines.join("\n")).toContain(`pid=${orphanPid}`);
      expect(lines.join("\n")).toContain("uptime=");

      expect(
        await waitForDeath(orphanPid, 2_000),
        `orphan pid ${orphanPid} survived the sweep`,
      ).toBe(true);
      expect(isAlive(adoptedPid), `pid ${adoptedPid} has a live parent and must not be swept`).toBe(
        true,
      );
    } finally {
      orphan?.dispose();
      adopted?.dispose();
      await rm(cacheRoot, { recursive: true, force: true });
    }
  }, 30_000);
});

interface PlantedProcess {
  pid: number;
  dispose(): void;
}

/**
 * Start `exe` from a shell so the test controls whether its parent stays alive.
 * With `keepParentAlive: false` the shell exits immediately and the kernel
 * reparents the child to pid 1, which is the shape a killed test runner leaves
 * behind.
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
  if (!options.keepParentAlive) {
    // Wait for the shell to go away, otherwise the child still has a live
    // parent and the sweep would correctly ignore it. "exit", not "close":
    // the backgrounded child inherits the shell's stdout pipe, so the stdio
    // streams stay open long after the shell itself is gone.
    await new Promise<void>((resolveExit) => shell.once("exit", () => resolveExit()));
    await waitUntil(() => parentOf(pid) === 1, 2_000);
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
