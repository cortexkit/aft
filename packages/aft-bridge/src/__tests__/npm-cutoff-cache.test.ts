import { expect, test } from "bun:test";
import {
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  watch,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { lspNpmInstallInvocation } from "../lsp-npm-policy.js";
import type { ResolvedNpm } from "../npm-resolver.js";
import { ManualClock } from "./test-utils/manual-clock.js";

interface ProbeMode {
  version: string;
  hang?: boolean;
  status?: number;
}

async function withProbe(
  mode: ProbeMode,
  run: (fixture: {
    npm: ResolvedNpm;
    clock: ManualClock;
    calls: () => string[][];
    setMode: (mode: ProbeMode) => void;
    touch: () => void;
    started: (count: number) => Promise<void>;
  }) => Promise<void>,
): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-cutoff-cache-"));
  const modePath = join(root, "mode.json");
  const trace = join(root, "argv.jsonl");
  const pids = join(root, "probe-pids");
  const script = join(root, "npm.mjs");
  const command = join(root, process.platform === "win32" ? "npm.cmd" : "npm");
  writeFileSync(modePath, JSON.stringify(mode));
  writeFileSync(trace, "");
  writeFileSync(pids, "");
  const source = `import { appendFileSync, readFileSync } from "node:fs";
const mode = JSON.parse(readFileSync(${JSON.stringify(modePath)}, "utf8"));
appendFileSync(${JSON.stringify(trace)}, JSON.stringify(process.argv.slice(2)) + "\\n");
appendFileSync(${JSON.stringify(pids)}, String(process.pid) + "\\n");
if (mode.hang) setInterval(() => {}, 1000);
else { console.log(mode.version); process.exitCode = mode.status ?? 0; }
`;
  writeFileSync(script, source);
  writeFileSync(
    command,
    process.platform === "win32"
      ? `@echo off\r\n"${process.execPath}" "${script}" %*\r\n`
      : `#!${process.execPath}\n${source}`,
    { mode: 0o755 },
  );
  const readPids = () => readFileSync(pids, "utf8").trim().split("\n").filter(Boolean).map(Number);
  try {
    await run({
      npm: { command, binDir: root },
      clock: new ManualClock(),
      calls: () =>
        readFileSync(trace, "utf8")
          .trim()
          .split("\n")
          .filter(Boolean)
          .map((line) => JSON.parse(line) as string[]),
      setMode: (next) => writeFileSync(modePath, JSON.stringify(next)),
      touch: () => {
        const before = statSync(command);
        utimesSync(command, before.atime, new Date(before.mtimeMs + 2000));
        expect(statSync(command).size).toBe(before.size);
        expect(statSync(command).mtimeMs).not.toBe(before.mtimeMs);
      },
      started: (count) =>
        new Promise<void>((resolve, reject) => {
          const deadline = setTimeout(() => {
            watcher.close();
            reject(new Error("npm probe did not start"));
          }, 10_000);
          const check = () => {
            if (readPids().length >= count) {
              clearTimeout(deadline);
              watcher.close();
              resolve();
            }
          };
          const watcher = watch(pids, check);
          check();
        }),
    });
  } finally {
    for (const pid of readPids()) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        /* The probe already exited. */
      }
    }
    rmSync(root, { recursive: true, force: true });
  }
}

const BEFORE = "2026-10-07T12:00:00.000Z";
const RETRY_MS = 5 * 60_000;

test("a transient npm timeout retries successfully after the five-minute backoff", async () => {
  await withProbe(
    { version: "11.6.0", hang: true },
    async ({ npm, clock, calls, setMode, started }) => {
      const pending = lspNpmInstallInvocation(npm, "server", BEFORE, clock);
      await started(1);
      clock.advance(5000);
      expect(await pending).toBeNull();
      setMode({ version: "11.6.0" });
      clock.advance(RETRY_MS - 1);
      expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).toBeNull();
      expect(calls()).toEqual([["--version"]]);
      clock.advance(1);
      expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).not.toBeNull();
      expect(calls()).toEqual([["--version"], ["--version"]]);
    },
  );
}, 15_000);

test("supported npm is retained until its executable mtime changes", async () => {
  await withProbe({ version: "11.6.0" }, async ({ npm, clock, calls, setMode, touch }) => {
    expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).not.toBeNull();
    setMode({ version: "6.14.0" });
    clock.advance(RETRY_MS * 2);
    expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).not.toBeNull();
    expect(calls()).toHaveLength(1);
    touch();
    expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).toBeNull();
    expect(calls()).toHaveLength(2);
  });
});

test("definite old-npm refusal is retained until executable replacement", async () => {
  await withProbe({ version: "6.14.0" }, async ({ npm, clock, calls, setMode, touch }) => {
    expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).toBeNull();
    setMode({ version: "11.6.0" });
    clock.advance(RETRY_MS * 2);
    expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).toBeNull();
    expect(calls()).toHaveLength(1);
    touch();
    expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).not.toBeNull();
    expect(calls()).toHaveLength(2);
  });
});

for (const mode of [{ version: "unknown" }, { version: "11.6.0", status: 1 }]) {
  test(`an unverifiable probe retries after backoff: ${mode.version}/${mode.status ?? 0}`, async () => {
    await withProbe(mode, async ({ npm, clock, calls, setMode }) => {
      expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).toBeNull();
      setMode({ version: "11.6.0" });
      clock.advance(RETRY_MS - 1);
      expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).toBeNull();
      expect(calls()).toEqual([["--version"]]);
      clock.advance(1);
      expect(await lspNpmInstallInvocation(npm, "server", BEFORE, clock)).not.toBeNull();
      expect(calls()).toEqual([["--version"], ["--version"]]);
    });
  });
}
