import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import {
  NpmTerminationUnknownError,
  npmInvocation,
  npmSpawnEnv,
  probeNpmVersion,
  resolveNpm,
  terminateNpmProcessTree,
} from "../npm-resolver.js";

/**
 * The resolver is dependency-injected (platform/env/home/execPath) so we can
 * build fake filesystem layouts and assert resolution order without touching
 * the real machine. These tests lock the behavior that fixes the GUI-launch
 * "npm not on PATH" auto-update failure.
 */
describe("resolveNpm", () => {
  let root: string;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "npm-resolver-test-"));
  });
  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
  });

  function makeNpm(dir: string, name = "npm"): string {
    mkdirSync(dir, { recursive: true });
    const p = join(dir, name);
    writeFileSync(p, "#!/usr/bin/env node\n");
    return p;
  }

  it("resolves npm from PATH first", () => {
    const pathDir = join(root, "path-bin");
    makeNpm(pathDir);
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: `/nonexistent${delimiter}${pathDir}` },
      home: root,
      execPath: "/usr/bin/node",
    });
    expect(result).not.toBeNull();
    expect(result?.binDir).toBe(pathDir);
    expect(result?.command).toBe(join(pathDir, "npm"));
  });

  it("falls back to node-adjacent npm when PATH has none", () => {
    const nodeBin = join(root, "node-install", "bin");
    makeNpm(nodeBin);
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: "/nonexistent" },
      home: root,
      execPath: join(nodeBin, "node"),
    });
    expect(result?.binDir).toBe(nodeBin);
  });

  it("falls back to nvm highest-version when PATH and node-adjacent both miss", () => {
    const nvm = join(root, ".nvm", "versions", "node");
    makeNpm(join(nvm, "v18.0.0", "bin"));
    const v20 = join(nvm, "v20.5.1", "bin");
    makeNpm(v20);
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: "/nonexistent" },
      home: root,
      execPath: "/standalone/bun", // no npm sibling
    });
    // Should pick the highest version (v20.5.1), not v18.
    expect(result?.binDir).toBe(v20);
  });

  it("honors NVM_BIN when set", () => {
    const nvmBin = join(root, "active-nvm-bin");
    makeNpm(nvmBin);
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: "/nonexistent", NVM_BIN: nvmBin },
      home: root,
      execPath: "/standalone/bun",
    });
    expect(result?.binDir).toBe(nvmBin);
  });

  it("falls back to an injected system dir when all else misses", () => {
    const sysDir = join(root, "sys-bin");
    makeNpm(sysDir);
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: "/nonexistent" },
      home: root,
      execPath: "/standalone/bun",
      systemNpmDirs: ["/nonexistent/system", sysDir],
    });
    expect(result?.binDir).toBe(sysDir);
  });

  it("resolves npm.cmd on win32", () => {
    const pathDir = join(root, "win-bin");
    makeNpm(pathDir, "npm.cmd");
    const result = resolveNpm({
      platform: "win32",
      env: { Path: pathDir },
      home: root,
      execPath: "C:\\node\\node.exe",
    });
    expect(result?.command).toBe(join(pathDir, "npm.cmd"));
  });

  it("does not treat case-variant Path as executable PATH on non-Windows", () => {
    const pathDir = join(root, "case-variant-bin");
    makeNpm(pathDir);

    expect(
      resolveNpm({
        platform: "linux",
        env: { Path: pathDir },
        home: root,
        execPath: "/standalone/bun",
        systemNpmDirs: [],
      }),
    ).toBeNull();
  });

  it("returns null when npm is nowhere to be found", () => {
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: "/nonexistent" },
      home: root, // empty tmp dir, no .nvm/.volta/etc
      execPath: "/standalone/bun",
      systemNpmDirs: [], // hermetic: don't pick up a real /usr/local/bin/npm on CI
    });
    expect(result).toBeNull();
  });

  it("ignores relative PATH entries (security: no '.' resolution)", () => {
    // A '.' or relative entry must never be honored.
    const result = resolveNpm({
      platform: "linux",
      env: { PATH: `.${delimiter}relative/bin` },
      home: root,
      execPath: "/standalone/bun",
      systemNpmDirs: [], // hermetic: ignore real system npm on CI
    });
    expect(result).toBeNull();
  });
});

describe("npmInvocation", () => {
  it("leaves Unix npm invocations direct", () => {
    expect(
      npmInvocation({ command: "/usr/bin/npm", binDir: "/usr/bin" }, ["install", "pkg"], "linux"),
    ).toEqual({
      command: "/usr/bin/npm",
      args: ["install", "pkg"],
    });
  });

  it("leaves native Windows executables direct", () => {
    expect(
      npmInvocation({ command: "C:\\tools\\npm.exe", binDir: "C:\\tools" }, ["--version"], "win32"),
    ).toEqual({
      command: "C:\\tools\\npm.exe",
      args: ["--version"],
    });
  });

  it("routes Windows cmd shims through ComSpec with a quoted command line", () => {
    const invocation = npmInvocation(
      {
        command: "C:\\Program Files\\nodejs\\npm.cmd",
        binDir: "C:\\Program Files\\nodejs",
      },
      ["install", "@scope/pkg@1.2.3", "argument with spaces"],
      "win32",
      { ComSpec: "C:\\Windows\\System32\\cmd.exe" },
    );

    expect(invocation).toEqual({
      command: "C:\\Windows\\System32\\cmd.exe",
      args: [
        "/d",
        "/s",
        "/v:off",
        "/c",
        '""%AFT_NPM_COMMAND%" "install" "@scope/pkg@1.2.3" "argument with spaces""',
      ],
      env: { AFT_NPM_COMMAND: "C:\\Program Files\\nodejs\\npm.cmd" },
      windowsVerbatimArguments: true,
      windowsCmdShim: true,
    });
  });

  it("rejects cmd arguments that would be expanded or terminate the command", () => {
    expect(() =>
      npmInvocation({ command: "C:\\nodejs\\npm.cmd", binDir: "C:\\nodejs" }, ["%PATH%"], "win32"),
    ).toThrow("cannot be represented safely");
  });

  it("waits for a direct child to report its actual exit", async () => {
    const child = new EventEmitter() as ChildProcess;
    let exitCode: number | null = null;
    const signals: Array<NodeJS.Signals | undefined> = [];
    Object.defineProperties(child, {
      pid: { value: 123 },
      exitCode: { get: () => exitCode },
      signalCode: { value: null },
      kill: {
        value: (signal?: NodeJS.Signals) => {
          signals.push(signal);
          return true;
        },
      },
    });

    let settled = false;
    const termination = terminateNpmProcessTree(
      child,
      { command: "/usr/bin/npm", args: ["install"] },
      process.env,
      1_000,
    ).then(() => {
      settled = true;
    });
    await Promise.resolve();

    expect(signals).toEqual([undefined]);
    expect(settled).toBe(false);
    exitCode = 0;
    child.emit("exit", 0, null);
    await termination;
    expect(settled).toBe(true);
  });

  it("force-kills a direct child after the termination grace period", async () => {
    const child = new EventEmitter() as ChildProcess;
    let exitCode: number | null = null;
    const signals: Array<NodeJS.Signals | undefined> = [];
    Object.defineProperties(child, {
      pid: { value: 123 },
      exitCode: { get: () => exitCode },
      signalCode: { value: null },
      kill: {
        value: (signal?: NodeJS.Signals) => {
          signals.push(signal);
          if (signal === "SIGKILL") {
            exitCode = 1;
            child.emit("exit", null, "SIGKILL");
          }
          return true;
        },
      },
    });

    await terminateNpmProcessTree(
      child,
      { command: "/usr/bin/npm", args: ["install"] },
      process.env,
      10,
    );
    expect(signals).toEqual([undefined, "SIGKILL"]);
  });

  it("fails closed when a direct child never reports exit after SIGKILL", async () => {
    const child = new EventEmitter() as ChildProcess;
    const signals: Array<NodeJS.Signals | undefined> = [];
    Object.defineProperties(child, {
      pid: { value: 123 },
      exitCode: { value: null },
      signalCode: { value: null },
      kill: {
        value: (signal?: NodeJS.Signals) => {
          signals.push(signal);
          return false;
        },
      },
    });

    await expect(
      terminateNpmProcessTree(
        child,
        { command: "/usr/bin/npm", args: ["install"] },
        process.env,
        10,
      ),
    ).rejects.toBeInstanceOf(NpmTerminationUnknownError);
    expect(signals).toEqual([undefined, "SIGKILL"]);
  });

  it("does not invoke taskkill for an already-successful cmd shim", async () => {
    const child = new EventEmitter() as ChildProcess;
    Object.defineProperties(child, {
      pid: { value: 123 },
      exitCode: { value: 0 },
      signalCode: { value: null },
    });

    await expect(
      terminateNpmProcessTree(
        child,
        { command: "cmd.exe", args: [], windowsCmdShim: true },
        { SystemRoot: join(tmpdir(), "aft-missing-system-root") },
        10,
      ),
    ).resolves.toBeUndefined();
  });

  it("accepts a successful cmd exit racing taskkill startup failure", async () => {
    const child = new EventEmitter() as ChildProcess;
    let exitCode: number | null = null;
    Object.defineProperties(child, {
      pid: { value: 123 },
      exitCode: { get: () => exitCode },
      signalCode: { value: null },
    });

    const termination = terminateNpmProcessTree(
      child,
      { command: "cmd.exe", args: [], windowsCmdShim: true },
      { SystemRoot: join(tmpdir(), "aft-missing-system-root") },
      100,
    );
    exitCode = 0;
    child.emit("exit", 0, null);

    await expect(termination).resolves.toBeUndefined();
  });

  it.skipIf(process.platform === "win32")(
    "accepts a successful cmd exit while taskkill remains pending past grace",
    async () => {
      const systemRoot = mkdtempSync(join(tmpdir(), "aft-hanging-taskkill-"));
      try {
        const system32 = join(systemRoot, "System32");
        const taskkill = join(system32, "taskkill.exe");
        mkdirSync(system32, { recursive: true });
        writeFileSync(taskkill, "#!/bin/sh\nexec sleep 10\n");
        chmodSync(taskkill, 0o755);

        const child = new EventEmitter() as ChildProcess;
        let exitCode: number | null = null;
        Object.defineProperties(child, {
          pid: { value: 123 },
          exitCode: { get: () => exitCode },
          signalCode: { value: null },
        });

        const termination = terminateNpmProcessTree(
          child,
          { command: "cmd.exe", args: [], windowsCmdShim: true },
          { SystemRoot: systemRoot },
          25,
        );
        exitCode = 0;
        child.emit("exit", 0, null);

        await expect(termination).resolves.toBeUndefined();
      } finally {
        rmSync(systemRoot, { recursive: true, force: true });
      }
    },
  );

  it.skipIf(process.platform === "win32")(
    "accepts confirmed taskkill when the child exit event lags past grace",
    async () => {
      const systemRoot = mkdtempSync(join(tmpdir(), "aft-successful-taskkill-"));
      try {
        const system32 = join(systemRoot, "System32");
        const taskkill = join(system32, "taskkill.exe");
        mkdirSync(system32, { recursive: true });
        writeFileSync(taskkill, "#!/bin/sh\nexit 0\n");
        chmodSync(taskkill, 0o755);

        const child = new EventEmitter() as ChildProcess;
        Object.defineProperties(child, {
          pid: { value: 123 },
          exitCode: { value: null },
          signalCode: { value: null },
        });

        // The fake child never exits, so this call resolves only at the grace
        // deadline, and only if taskkill's exit event has been delivered by
        // then. Under Bun 1.4.0 on macOS a fast-exiting child's exit event is
        // occasionally delivered about 1 s late (measured 2-5 per 200 spawns
        // of this exact shape; 0 per 200 under Node 24), so a 1 s grace reads
        // that runtime hiccup as "taskkill did not confirm". 3 s keeps the
        // assertion about the code's late-exit handling, not the runtime's.
        await expect(
          terminateNpmProcessTree(
            child,
            { command: "cmd.exe", args: [], windowsCmdShim: true },
            { SystemRoot: systemRoot },
            3_000,
          ),
        ).resolves.toBeUndefined();
      } finally {
        rmSync(systemRoot, { recursive: true, force: true });
      }
    },
  );

  it("fails closed when taskkill cannot start", async () => {
    const child = new EventEmitter() as ChildProcess;
    const signals: Array<NodeJS.Signals | undefined> = [];
    Object.defineProperties(child, {
      pid: { value: 123 },
      exitCode: { value: null },
      signalCode: { value: null },
      kill: {
        value: (signal?: NodeJS.Signals) => {
          signals.push(signal);
          return true;
        },
      },
    });

    let caught: unknown;
    try {
      await terminateNpmProcessTree(
        child,
        { command: "cmd.exe", args: [], windowsCmdShim: true },
        { SystemRoot: join(tmpdir(), "aft-missing-system-root") },
        10,
      );
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(NpmTerminationUnknownError);
    expect((caught as NpmTerminationUnknownError).code).toBe("npm_termination_unknown");
    expect(signals).toEqual([]);
  });

  it.skipIf(process.platform !== "win32")(
    "executes a cmd shim whose absolute path contains spaces",
    () => {
      const root = mkdtempSync(join(tmpdir(), "npm invocation "));
      try {
        const shim = join(root, "npm.cmd");
        writeFileSync(shim, "@echo off\r\necho [%~1] [%~2]\r\n");
        const invocation = npmInvocation({ command: shim, binDir: root }, ["hello world", "plain"]);
        const result = spawnSync(invocation.command, invocation.args, {
          encoding: "utf8",
          env: { ...process.env, ...invocation.env },
          windowsVerbatimArguments: invocation.windowsVerbatimArguments,
        });

        expect(result.error).toBeUndefined();
        expect(result.status).toBe(0);
        expect(result.stdout.trim()).toBe("[hello world] [plain]");
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  );

  it.skipIf(process.platform !== "win32")(
    "probes npm.cmd from a path containing a literal percent",
    () => {
      const root = mkdtempSync(join(tmpdir(), "npm %TEMP% probe "));
      try {
        const shim = join(root, "npm.cmd");
        writeFileSync(shim, "@echo off\r\necho 9.8.7\r\n");
        expect(probeNpmVersion({ command: shim, binDir: root })).toBe("9.8.7");
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  );

  it.skipIf(process.platform !== "win32")(
    "terminates the node descendant of a cmd shim",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "npm tree kill "));
      try {
        const shim = join(root, "npm.cmd");
        const started = join(root, "npm-descendant-started");
        const sentinel = join(root, "npm-still-running");
        writeFileSync(
          shim,
          "@echo off\r\n\"%AFT_TEST_NODE%\" -e \"require('fs').writeFileSync(process.argv[1], 'started'); setTimeout(function(){require('fs').writeFileSync(process.argv[2], 'leaked')}, 1000)\" \"%~1\" \"%~2\"\r\n",
        );
        const invocation = npmInvocation({ command: shim, binDir: root }, [started, sentinel]);
        const child = spawn(invocation.command, invocation.args, {
          env: {
            ...process.env,
            PATH: "",
            AFT_TEST_NODE: process.execPath,
            ...invocation.env,
          },
          stdio: "ignore",
          windowsVerbatimArguments: invocation.windowsVerbatimArguments,
        });
        let terminated = false;
        try {
          const startDeadline = Date.now() + 3_000;
          while (!existsSync(started) && Date.now() < startDeadline) {
            await new Promise((resolve) => setTimeout(resolve, 25));
          }
          expect(existsSync(started)).toBe(true);
          await terminateNpmProcessTree(child, invocation);
          terminated = true;
          await new Promise((resolve) => setTimeout(resolve, 1_100));
          expect(existsSync(sentinel)).toBe(false);
        } finally {
          if (!terminated) await terminateNpmProcessTree(child, invocation);
        }
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
});

describe("npmSpawnEnv", () => {
  it("prepends binDir to PATH so npm finds its sibling node", () => {
    const env = npmSpawnEnv(
      { command: "/opt/homebrew/bin/npm", binDir: "/opt/homebrew/bin" },
      { PATH: "/usr/bin" },
    );
    expect(env.PATH).toBe(`/opt/homebrew/bin${delimiter}/usr/bin`);
  });

  it("sets PATH to binDir alone when base PATH is empty", () => {
    const env = npmSpawnEnv({ command: "/opt/homebrew/bin/npm", binDir: "/opt/homebrew/bin" }, {});
    expect(env.PATH).toBe("/opt/homebrew/bin");
  });

  it("leaves env unchanged when binDir is null (PATH-resolved)", () => {
    const env = npmSpawnEnv({ command: "npm", binDir: null }, { PATH: "/usr/bin" });
    expect(env.PATH).toBe("/usr/bin");
  });

  it("normalizes inherited Windows Path spelling at the npm spawn seam", () => {
    const env = npmSpawnEnv(
      { command: "C:\\nodejs\\npm.cmd", binDir: "C:\\nodejs" },
      { Path: "C:\\Windows\\System32", PATH: "C:\\stale" },
      "win32",
    );

    expect(Object.keys(env).filter((key) => key.toLowerCase() === "path")).toEqual(["Path"]);
    expect(env.Path).toBe("C:\\nodejs;C:\\Windows\\System32");
  });
});
