/**
 * Tests for `validateExtraction` in lsp-github-install.ts.
 *
 * Total uncompressed bytes capped at MAX_EXTRACT_BYTES (1 GiB).
 * Mirrors the OpenCode plugin tests.
 */

import { afterEach, describe, expect, spyOn, test } from "bun:test";
import * as childProcess from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync, spawnSync } from "@cortexkit/aft-bridge";
import {
  _extractArchiveSafelyForTesting as extractArchiveSafely,
  _precheckArchiveSizeForTesting as precheckArchiveSize,
  _runArchiveToolForTesting as runArchiveTool,
  validateExtraction,
} from "../lsp-github-install.js";

const tempRoots = new Set<string>();

function createTarFixture(archivePath: string, source: string): void {
  execFileSync("tar", ["-czf", archivePath, "-C", source, "bin"], {
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 1_000,
    killSignal: "SIGKILL",
  });
}

async function withinTestDeadline<T>(operation: Promise<T>): Promise<T> {
  let deadline: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<T>((_, reject) => {
        deadline = setTimeout(
          () => reject(new Error("archive operation exceeded 1000 ms test deadline")),
          1_000,
        );
      }),
    ]);
  } finally {
    clearTimeout(deadline);
  }
}

function createStagingFixture(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-extract-tests-"));
  tempRoots.add(root);
  return root;
}

async function withArchiveDiagnostics(operation: () => Promise<void>): Promise<void> {
  const original = childProcess.execFile;
  const events: string[] = [];
  const children = new Set<childProcess.ChildProcess>();
  const started = performance.now();
  const record = (event: string): void => {
    events.push(`${Math.round(performance.now() - started)}ms ${event}`);
  };
  const report = (): void => {
    console.error("Archive child lifecycle:", events.join("\n"));
    for (const child of children) {
      console.error({
        pid: child.pid,
        exitCode: child.exitCode,
        stdinClosed: child.stdin?.writableEnded,
        stdoutEnded: child.stdout?.readableEnded,
        stderrEnded: child.stderr?.readableEnded,
      });
    }
  };
  const toolMock = spyOn(childProcess, "execFile").mockImplementation(((
    command: string,
    args: string[],
    options: childProcess.ExecFileOptionsWithStringEncoding,
    callback: (
      error: childProcess.ExecFileException | null,
      stdout: string,
      stderr: string,
    ) => void,
  ) => {
    const label = `${command} ${args[0]}`;
    record(`${label} called`);
    let finished = false;
    const complete = (
      error: childProcess.ExecFileException | null,
      stdout: string,
      stderr: string,
    ): void => {
      if (finished) return;
      finished = true;
      clearTimeout(deadline);
      record(`${label} callback${error ? `: ${error.message}` : ""}`);
      callback(error, stdout, stderr);
    };
    // Fail the owning test even if native child close/callback delivery stalls.
    const deadline = setTimeout(() => {
      report();
      complete(new Error(`${label} test child timed out after 1000 ms`), "", "");
      child.kill("SIGKILL");
      child.stdin?.destroy();
      child.stdout?.destroy();
      child.stderr?.destroy();
    }, 1_000);
    const child = original(
      command,
      args,
      { ...options, timeout: 1_000, killSignal: "SIGKILL" },
      complete,
    );
    children.add(child);
    child.once("spawn", () => record(`${label} spawn pid=${child.pid}`));
    child.stdout?.once("end", () => record(`${label} stdout end`));
    child.stderr?.once("end", () => record(`${label} stderr end`));
    child.once("exit", (code, signal) => record(`${label} exit ${code}/${signal}`));
    child.once("close", (code, signal) => {
      record(`${label} close ${code}/${signal}`);
      children.delete(child);
    });
    return child;
  }) as typeof childProcess.execFile);
  try {
    await operation();
  } catch (error) {
    report();
    throw error;
  } finally {
    toolMock.mockRestore();
  }
}

afterEach(() => {
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("precheckArchiveSize", () => {
  test("rejects an archive TOC with a sparse-sized member before extraction", async () => {
    const root = createStagingFixture();
    const archivePath = join(root, "bomb.zip");
    const oversizedSize = 1_073_741_825;
    writeFakeZipWithUncompressedSize(archivePath, oversizedSize);
    const calls: string[][] = [];
    const child = { stdin: { end: () => {} } } as unknown as childProcess.ChildProcess;
    // Keep the size assertion independent of how long the host takes to start unzip.
    const toolMock = spyOn(childProcess, "execFile").mockImplementation(((
      command: string,
      args: string[],
      _options: unknown,
      callback: (
        error: childProcess.ExecFileException | null,
        stdout: string,
        stderr: string,
      ) => void,
    ) => {
      calls.push(args);
      const listing = [
        `Archive:  ${archivePath}`,
        "  Length      Date    Time    Name",
        "---------  ---------- -----   ----",
        `${oversizedSize}  1980-01-01 00:00   sparse.bin`,
        "---------                     -------",
        `${oversizedSize}                     1 file`,
        "",
      ].join("\n");
      if (args[0] === "-l") {
        expect(command).toBe("unzip");
        callback(null, listing, "");
      } else if (args[0] === "-Z1") {
        expect(command).toBe("unzip");
        callback(null, "sparse.bin\n", "");
      } else if (args[0] === "-tvf") {
        callback(null, `-rw-r--r-- 0 user group ${oversizedSize} Jan 1 00:00 sparse.bin\n`, "");
      } else if (args[0] === "-tf") callback(null, "sparse.bin\n", "");
      else callback(new Error(`unexpected archive tool arguments: ${args.join(" ")}`), "", "");
      return child;
    }) as typeof childProcess.execFile);

    try {
      await expect(precheckArchiveSize(archivePath, "zip")).rejects.toThrow(
        /archive uncompressed size/,
      );
      expect(calls).toEqual(
        process.platform === "win32"
          ? [
              ["-tvf", archivePath],
              ["-tf", archivePath],
            ]
          : [
              ["-l", archivePath],
              ["-Z1", archivePath],
            ],
      );
    } finally {
      toolMock.mockRestore();
    }
  });
});

describe("extractArchiveSafely", () => {
  // Hold each archive-tool callback until a timer has run. This measures the
  // host's event loop, not how quickly tar exits or Bun delivers pipe EOF.
  // An unbounded setImmediate spin also competes with the I/O being observed.
  test("extracts without blocking the event loop", async () => {
    const root = createStagingFixture();
    const source = join(root, "source");
    mkdirSync(join(source, "bin"), { recursive: true });
    writeFileSync(join(source, "bin", "server"), "binary");
    const archivePath = join(root, "server.tar.gz");
    createTarFixture(archivePath, source);
    const dest = join(root, "extract");

    type PendingTool = { command: string; args: string[]; complete: () => void };
    let announce: (tool: PendingTool) => void;
    const nextTool = (): Promise<PendingTool> =>
      new Promise((resolve) => {
        announce = resolve;
      });
    let started = nextTool();
    let draining = false;
    let releasePending: (() => void) | undefined;
    const toolMock = spyOn(childProcess, "execFile").mockImplementation(((
      command: string,
      args: string[],
      _options: unknown,
      callback: (
        error: childProcess.ExecFileException | null,
        stdout: string,
        stderr: string,
      ) => void,
    ) => {
      const complete = (): void => {
        releasePending = undefined;
        // Complete the controlled tool without waiting for a native child.
        // The separate integration case exercises real tar and its pipes.
        try {
          if (args[0] === "-tvf")
            callback(null, "-rw-r--r-- 0 user group 6 Jan 1 00:00 bin/server\n", "");
          else if (args[0] === "-tf") callback(null, "bin/server\n", "");
          else if (args[0] === "-xzf") {
            const staging = args[args.indexOf("-C") + 1];
            mkdirSync(join(staging, "bin"), { recursive: true });
            writeFileSync(join(staging, "bin", "server"), "binary");
            callback(null, "", "");
          } else throw new Error(`unexpected archive tool arguments: ${args.join(" ")}`);
        } catch (error) {
          callback(error as childProcess.ExecFileException, "", "");
        }
      };
      releasePending = complete;
      if (draining) complete();
      else announce({ command, args, complete });
      // runArchiveTool consumes the callback, not the ChildProcess handle.
      return {} as childProcess.ChildProcess;
    }) as typeof childProcess.execFile);
    let settled = false;
    const extraction = withinTestDeadline(
      extractArchiveSafely(archivePath, dest, "tar.gz"),
    ).finally(() => {
      settled = true;
    });
    const finished = extraction.then(() => null);
    try {
      for (const flag of ["-tvf", "-tf", "-xzf"]) {
        const pending = await Promise.race([started, finished]);
        expect(pending, `extraction completed before the asynchronous ${flag} call`).not.toBeNull();
        if (!pending) return;
        expect(pending.args[0]).toBe(flag);
        await new Promise<void>((resolve) => setTimeout(resolve, 0));
        expect(settled, `extraction settled before a timer ran during ${flag}`).toBe(false);
        started = nextTool();
        pending.complete();
      }
      await extraction;
    } finally {
      draining = true;
      releasePending?.();
      await extraction.catch(() => {});
      toolMock.mockRestore();
    }

    expect(readFileSync(join(dest, "bin", "server"), "utf8")).toBe("binary");
  });

  test("extracts a valid archive with the real asynchronous tools", async () => {
    const root = createStagingFixture();
    const source = join(root, "source");
    mkdirSync(join(source, "bin"), { recursive: true });
    writeFileSync(join(source, "bin", "server"), "binary");
    const archivePath = join(root, "server.tar.gz");
    createTarFixture(archivePath, source);
    const dest = join(root, "extract");

    await withArchiveDiagnostics(() => extractArchiveSafely(archivePath, dest, "tar.gz"));

    expect(readFileSync(join(dest, "bin", "server"), "utf8")).toBe("binary");
  });

  test("a failed extraction leaves no staging directory behind", async () => {
    const root = createStagingFixture();
    const archivePath = join(root, "broken.tar.gz");
    writeFileSync(archivePath, "not an archive");
    const dest = join(root, "extract");

    await expect(
      withArchiveDiagnostics(() => extractArchiveSafely(archivePath, dest, "tar.gz")),
    ).rejects.toThrow();
    expect(existsSync(dest)).toBe(false);
    const leftovers = readdirSync(root).filter((entry) => entry.startsWith("extract.staging-"));
    expect(leftovers).toEqual([]);
  });
});

describe("archive tool deadlines", () => {
  test.each([
    "tar.gz",
    "tar.xz",
    "zip",
  ])("bounds every %s list and extraction command", async (archiveType) => {
    const calls: { args: string[]; options: childProcess.ExecFileOptionsWithStringEncoding }[] = [];
    const end = () => {};
    const child = { stdin: { end } } as unknown as childProcess.ChildProcess;
    const toolMock = spyOn(childProcess, "execFile").mockImplementation(((
      _command: string,
      args: string[],
      options: childProcess.ExecFileOptionsWithStringEncoding,
      callback: (error: null, stdout: string, stderr: string) => void,
    ) => {
      calls.push({ args, options });
      callback(null, "", "");
      return child;
    }) as typeof childProcess.execFile);
    const stdin = spyOn(child.stdin!, "end");
    const root = createStagingFixture();
    try {
      await extractArchiveSafely(join(root, "archive"), join(root, "extract"), archiveType);
      expect(calls.map(({ options }) => options.timeout)).toEqual([30_000, 30_000, 180_000]);
      expect(calls.every(({ options }) => options.killSignal === "SIGKILL")).toBe(true);
      expect(stdin).toHaveBeenCalledTimes(3);
    } finally {
      toolMock.mockRestore();
    }
  });

  test("kills a real archive child at its deadline", async () => {
    await expect(
      withinTestDeadline(
        runArchiveTool(process.execPath, ["-e", "setInterval(() => {}, 1000)"], 30),
      ),
    ).rejects.toThrow(/timed out after 30 ms/);
  });

  test("the real-tool test watchdog reports a stalled child", async () => {
    await expect(
      withArchiveDiagnostics(async () => {
        await runArchiveTool(process.execPath, ["-e", "setInterval(() => {}, 1000)"], 30_000);
      }),
    ).rejects.toThrow(/test child timed out after 1000 ms/);
  });

  test.skipIf(process.platform === "win32")(
    "a hung tar fixture fails by name before the runner deadline",
    () => {
      const root = createStagingFixture();
      const pidFile = join(root, "tar.pid");
      writeFileSync(
        join(root, "tar"),
        '#!/bin/sh\n[ -n "$AFT_HANG_WARM" ] && exit 0\necho $$ > "$AFT_HANG_PID"\nexec sleep 60\n',
        { mode: 0o755 },
      );
      // macOS checks a newly written executable on its first run, which can take
      // longer than the inner test's deadline on a loaded machine. Run it once
      // so the timed run below starts the fake tar promptly.
      execFileSync(join(root, "tar"), [], { env: { ...process.env, AFT_HANG_WARM: "1" } });
      try {
        // A separate runner is essential: Bun cannot time out a synchronous fixture.
        const result = spawnSync(
          process.execPath,
          [
            "test",
            import.meta.path,
            "--timeout",
            "5000",
            "-t",
            "extracts without blocking the event loop",
          ],
          {
            env: { ...process.env, PATH: `${root}:${process.env.PATH}`, AFT_HANG_PID: pidFile },
            encoding: "utf8",
            stdio: ["ignore", "pipe", "pipe"],
            timeout: 3_000,
            killSignal: "SIGKILL",
          },
        );
        expect(
          result.error,
          "the fixture must fail itself, not hang until the outer runner is killed",
        ).toBeUndefined();
        expect(existsSync(pidFile), "the fake tar must actually run").toBe(true);
        expect(result.status).not.toBe(0);
        expect(result.stderr).toContain(
          "(fail) extractArchiveSafely > extracts without blocking the event loop",
        );
        expect(result.stderr).toMatch(/ETIMEDOUT|timed out/);
      } finally {
        if (existsSync(pidFile)) {
          try {
            process.kill(Number(readFileSync(pidFile, "utf8")), "SIGKILL");
          } catch {
            /* The fixture deadline may already have killed tar. */
          }
        }
      }
    },
  );

  test("rejects when child exit never delivers pipe EOF", async () => {
    const child = {
      kill: () => true,
      stdin: { end: () => {}, destroy: () => {} },
      stdout: { destroy: () => {} },
      stderr: { destroy: () => {} },
    };
    const kill = spyOn(child, "kill");
    const stdout = spyOn(child.stdout, "destroy");
    const stderr = spyOn(child.stderr, "destroy");
    // Model an exited child whose inherited output descriptors never close.
    const toolMock = spyOn(childProcess, "execFile").mockReturnValue(
      child as unknown as childProcess.ChildProcess,
    );
    try {
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        await expect(
          Promise.race([
            runArchiveTool("tar", ["-tf", "held-pipe.tar"], 30),
            new Promise<string>((_, reject) => {
              timer = setTimeout(
                () => reject(new Error("archive promise still pending after 200 ms")),
                200,
              );
            }),
          ]),
        ).rejects.toThrow(/tar.*-tf.*timed out.*30/);
      } finally {
        clearTimeout(timer);
      }
      expect(kill).toHaveBeenCalledWith("SIGKILL");
      expect(stdout).toHaveBeenCalled();
      expect(stderr).toHaveBeenCalled();
    } finally {
      toolMock.mockRestore();
    }
  }, 500);
});

function writeFakeZipWithUncompressedSize(path: string, uncompressedSize: number): void {
  const name = Buffer.from("sparse.bin");
  const local = Buffer.alloc(30 + name.length);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(20, 4);
  local.writeUInt32LE(0, 14);
  local.writeUInt32LE(uncompressedSize, 22);
  local.writeUInt16LE(name.length, 26);
  name.copy(local, 30);

  const central = Buffer.alloc(46 + name.length);
  central.writeUInt32LE(0x02014b50, 0);
  central.writeUInt16LE(20, 4);
  central.writeUInt16LE(20, 6);
  central.writeUInt32LE(0, 16);
  central.writeUInt32LE(uncompressedSize, 24);
  central.writeUInt16LE(name.length, 28);
  name.copy(central, 46);

  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(1, 8);
  end.writeUInt16LE(1, 10);
  end.writeUInt32LE(central.length, 12);
  end.writeUInt32LE(local.length, 16);
  writeFileSync(path, Buffer.concat([local, central, end]));
}

describe("validateExtraction (Pi)", () => {
  test("accepts a normal extraction", () => {
    const staging = createStagingFixture();
    mkdirSync(join(staging, "bin"), { recursive: true });
    writeFileSync(join(staging, "bin", "lsp-binary"), "binary");
    expect(() => validateExtraction(staging)).not.toThrow();
  });

  test("rejects symlinks (zip-slip defense)", () => {
    const staging = createStagingFixture();
    writeFileSync(join(staging, "real.txt"), "real");
    symlinkSync("real.txt", join(staging, "link.txt"));
    expect(() => validateExtraction(staging)).toThrow(/symlink.*zip-slip defense/);
  });

  test("rejects sparse file > 1 GiB cap (decompression bomb)", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
    const fh = fs.openSync(join(staging, "sparse.bin"), "w");
    try {
      fs.ftruncateSync(fh, 1024 * 1024 * 1024 + 1);
    } finally {
      fs.closeSync(fh);
    }
    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });

  test("rejects accumulated bytes across files exceeding cap", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
    for (const name of ["a.bin", "b.bin"]) {
      const fh = fs.openSync(join(staging, name), "w");
      try {
        fs.ftruncateSync(fh, 600 * 1024 * 1024);
      } finally {
        fs.closeSync(fh);
      }
    }
    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });
});
