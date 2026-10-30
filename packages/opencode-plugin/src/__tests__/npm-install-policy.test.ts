import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  type AutoInstallConfig,
  abortInFlightAutoInstalls,
  runAutoInstall,
} from "../lsp-auto-install.js";
import { lspPackageDir } from "../lsp-cache.js";
import { NPM_LSP_TABLE } from "../lsp-npm-table.js";

async function recordInstall(
  pinned: boolean,
  npmVersion = "11.6.0",
  options: {
    servers?: readonly string[];
    probeDelayMs?: number;
    hangProbe?: boolean;
    passes?: number;
    observePending?: boolean;
    abortWhilePending?: boolean;
  } = {},
) {
  const root = mkdtempSync(join(tmpdir(), "aft-npm-argv-"));
  const bin = join(root, "bin");
  const project = join(root, "project");
  const trace = join(root, "argv.jsonl");
  const pids = join(root, "probe-pids");
  mkdirSync(bin);
  mkdirSync(project);
  for (const name of ["main.py", "config.yaml", "main.sh"]) {
    writeFileSync(join(project, name), "fixture\n");
  }
  writeFileSync(trace, "");
  writeFileSync(pids, "");
  const readCalls = () =>
    readFileSync(trace, "utf8")
      .trim()
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line) as string[]);
  const readPids = () => readFileSync(pids, "utf8").trim().split("\n").filter(Boolean).map(Number);
  const alive = (pid: number) => {
    try {
      process.kill(pid, 0);
      return true;
    } catch {
      return false;
    }
  };
  const script = join(bin, "npm.mjs");
  const source = `import { appendFileSync } from "node:fs";
const args = process.argv.slice(2);
appendFileSync(${JSON.stringify(trace)}, JSON.stringify(args) + "\\n");
if (args[0] === "--version") {
  appendFileSync(${JSON.stringify(pids)}, String(process.pid) + "\\n");
  if (${options.hangProbe ?? false}) setInterval(() => {}, 1000);
  else setTimeout(() => console.log(${JSON.stringify(npmVersion)}), ${options.probeDelayMs ?? 0});
}
`;
  writeFileSync(script, source);
  writeFileSync(
    join(bin, process.platform === "win32" ? "npm.cmd" : "npm"),
    process.platform === "win32"
      ? `@echo off\r\n"${process.execPath}" "${script}" %*\r\n`
      : `#!${process.execPath}\n${source}`,
    { mode: 0o755 },
  );
  try {
    return await withEnv(
      { AFT_CACHE_DIR: join(root, "cache"), PATH: `${bin}${delimiter}${process.env.PATH}` },
      async () => {
        const started = Date.now();
        const servers = new Set(options.servers ?? ["pyright"]);
        const config: AutoInstallConfig = {
          autoInstall: true,
          graceDays: 3,
          versions: pinned ? { pyright: "1.2.3" } : {},
          disabled: new Set(
            NPM_LSP_TABLE.filter((spec) => !servers.has(spec.npm)).map((s) => s.id),
          ),
        };
        let fetchCount = 0;
        const fetchImpl = (async () => {
          if (options.servers) {
            await new Promise((resolve) => setTimeout(resolve, ++fetchCount * 10));
          }
          return new Response(
            JSON.stringify({
              time: { "1.2.3": "2020-01-01T00:00:00Z" },
              "dist-tags": { latest: "1.2.3" },
            }),
          );
        }) as typeof fetch;
        const passCalls: string[][][] = [];
        let timerSawPending: boolean | undefined;
        for (let pass = 0; pass < (options.passes ?? 1); pass++) {
          const result = runAutoInstall(project, config, fetchImpl);
          let settled = false;
          const completion = result.installsComplete.then(() => {
            settled = true;
          });
          if (pass === 0 && (options.observePending || options.abortWhilePending)) {
            await new Promise<void>((resolve) =>
              setTimeout(() => {
                timerSawPending = !settled;
                if (options.abortWhilePending) void abortInFlightAutoInstalls();
                resolve();
              }, 20),
            );
          }
          await completion;
          passCalls.push(readCalls());
        }
        return {
          calls: readCalls(),
          passCalls,
          started,
          finished: Date.now(),
          timerSawPending,
          probeAlive: readPids().some(alive),
          anchors: [...servers].filter((name) =>
            existsSync(join(lspPackageDir(name), "package.json")),
          ),
        };
      },
    );
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

describe("OpenCode npm installer policy", () => {
  test("real unpinned install carries the publication cutoff and suppresses scripts", async () => {
    const { calls, started, finished } = await recordInstall(false);
    expect(calls).toHaveLength(2);
    expect(calls[0]).toEqual(["--version"]);
    const install = calls[1];
    const cutoff = install?.[4];
    expect(cutoff).toMatch(/^--before=/);
    const timestamp = Date.parse(cutoff.slice("--before=".length));
    expect(timestamp).toBeGreaterThanOrEqual(started - 3 * 86_400_000);
    expect(timestamp).toBeLessThanOrEqual(finished - 3 * 86_400_000);
    expect(install).toEqual([
      "install",
      "--no-save",
      "--ignore-scripts",
      "--silent",
      cutoff,
      "pyright@1.2.3",
    ]);
  });

  test("real explicit pin bypasses the cutoff and npm version probe", async () => {
    const { calls } = await recordInstall(true, "6.14.0");
    expect(calls).toEqual([
      ["install", "--no-save", "--ignore-scripts", "--silent", "pyright@1.2.3"],
    ]);
  });

  test("real unpinned install refuses npm without cutoff support", async () => {
    const { calls } = await recordInstall(false, "6.14.0");
    expect(calls).toEqual([["--version"]]);
  });

  test("concurrent installs and later passes share one npm capability probe", async () => {
    const servers = ["pyright", "yaml-language-server", "bash-language-server"];
    const { calls, passCalls } = await recordInstall(false, "11.6.0", {
      servers,
      probeDelayMs: 100,
      passes: 2,
    });
    expect(passCalls[0].filter((args) => args[0] === "install")).toHaveLength(servers.length);
    expect(calls.filter((args) => args[0] === "--version")).toEqual([["--version"]]);
    const installs = calls.filter((args) => args[0] === "install");
    expect(installs).toHaveLength(servers.length * 2);
    expect(installs.map((args) => args.at(-1)).sort()).toEqual(
      [...servers, ...servers].map((name) => `${name}@1.2.3`).sort(),
    );
    for (const args of installs) {
      expect(args).toContain("--ignore-scripts");
      expect(args.some((arg) => arg.startsWith("--before="))).toBe(true);
    }
  });

  test("a hung npm probe leaves the event loop responsive and caches refusal", async () => {
    const result = await recordInstall(false, "11.6.0", {
      hangProbe: true,
      observePending: true,
      passes: 2,
    });
    expect(result.timerSawPending).toBe(true);
    expect(result.calls).toEqual([["--version"]]);
    expect(result.probeAlive).toBe(false);
    expect(result.anchors).toEqual([]);
  }, 20_000);

  test("shutdown during an npm probe does not start an install after the probe", async () => {
    const result = await recordInstall(false, "11.6.0", {
      probeDelayMs: 150,
      abortWhilePending: true,
    });
    expect(result.timerSawPending).toBe(true);
    expect(result.calls).toEqual([["--version"]]);
    expect(result.anchors).toEqual([]);
  });
});
