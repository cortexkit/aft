/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setActiveLogger } from "../active-logger.js";
import { BinaryBridge } from "../bridge.js";
import type { Logger, LogMeta } from "../logger.js";
import { BridgePool } from "../pool.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-transport-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function writeExecutable(name: string, source: string): string {
  const path = join(workDir, name);
  writeFileSync(path, source);
  chmodSync(path, 0o755);
  return path;
}

describe("BinaryBridge transport regressions", () => {
  test("stdout NDJSON decoder preserves multibyte UTF-8 split across chunks", async () => {
    const script = writeExecutable(
      "split-emoji.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let input = "";
process.stdin.on("data", (chunk) => {
  input += chunk;
  const newline = input.indexOf("\\n");
  if (newline === -1) return;
  const line = input.slice(0, newline);
  const req = JSON.parse(line);
  const out = Buffer.from(JSON.stringify({ id: req.id, success: true, version: "1.2.3 🚀" }) + "\\n");
  const emoji = Buffer.from("🚀");
  const splitAt = out.indexOf(emoji) + 1;
  process.stdout.write(out.subarray(0, splitAt));
  setTimeout(() => process.stdout.write(out.subarray(splitAt)), 5);
});
`,
    );
    // Generous transport budget: node spawn alone can take ~300ms on a cold
    // cache, and we have a 5ms intentional delay inside the fixture before
    // the second chunk arrives. Anything under 1s is flaky on shared runners.
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });

    try {
      const response = await bridge.send("version");
      expect(response.version).toBe("1.2.3 🚀");
    } finally {
      await bridge.shutdown();
    }
  });

  test("timeout-killed bridge aborts sibling requests immediately", async () => {
    const script = writeExecutable(
      "silent.js",
      `#!/usr/bin/env node
process.stdin.resume();
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });

    try {
      // Attach the handlers BEFORE the await so both rejections are captured
      // even if Bun's `expect(p).rejects` re-throws the first error and skips
      // the second promise.
      const firstResult = bridge.send("version", {}, { timeoutMs: 20 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      const siblingResult = bridge.send("version", {}, { timeoutMs: 1_000 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );

      // The first request's 20ms timer fires before the sibling's 1s timer.
      // The Oracle F2 fix requires the sibling to reject immediately (not wait
      // its own 1s timer) with a sibling-abort error. We assert via wall-clock
      // by racing both against a 100ms ceiling — well below the sibling's 1s
      // budget, so a passing test confirms the sibling did NOT wait its timer.
      const [first, sibling] = (await Promise.race([
        Promise.all([firstResult, siblingResult]),
        new Promise<[string, string]>((resolve) =>
          setTimeout(() => resolve(["pending", "pending"]), 200),
        ),
      ])) as [string, string];

      // F2 contract:
      //  - first request rejects with a timeout error (its own timer fired)
      //  - sibling request rejects with the sibling-abort error (NOT its own
      //    1s timer — that would still be pending at the 200ms ceiling)
      expect(first).toMatch(/timed out|aborted/);
      expect(sibling).toContain("sibling timeout");
    } finally {
      await bridge.shutdown();
    }
  });

  test("version RPC success:false rejects when minVersion is set", async () => {
    const bridge = new BinaryBridge("/fake/aft", workDir, { minVersion: "1.0.0" });
    const testBridge = bridge as unknown as {
      send(command: string): Promise<Record<string, unknown>>;
      checkVersion(): Promise<void>;
    };
    testBridge.send = async () => ({ success: false, code: "unknown-command" });

    await expect(testBridge.checkVersion()).rejects.toThrow(/Binary version check failed/);
  });

  test("version RPC missing version rejects when minVersion is set", async () => {
    const bridge = new BinaryBridge("/fake/aft", workDir, { minVersion: "1.0.0" });
    const testBridge = bridge as unknown as {
      send(command: string): Promise<Record<string, unknown>>;
      checkVersion(): Promise<void>;
    };
    testBridge.send = async () => ({ success: true });

    await expect(testBridge.checkVersion()).rejects.toThrow(/did not report a version/);
  });

  test("version mismatch callback can swap binaries and retry the original request once", async () => {
    const compatible = writeExecutable(
      "compatible.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [] }) + "\\n");
    } else if (req.command === "version") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, version: "2.0.0" }) + "\\n");
    } else {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, source: "compatible", command: req.command }) + "\\n");
    }
  }
});
`,
    );
    const stale = writeExecutable(
      "stale.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [] }) + "\\n");
    } else if (req.command === "version") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, version: "0.1.0" }) + "\\n");
      setTimeout(() => process.exit(1), 25);
    } else {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, source: "stale", command: req.command }) + "\\n");
    }
  }
});
`,
    );
    let mismatchCalls = 0;
    const bridge = new BinaryBridge(stale, workDir, {
      timeoutMs: 5_000,
      maxRestarts: 0,
      minVersion: "1.0.0",
      onVersionMismatch: async (binaryVersion, minVersion) => {
        mismatchCalls++;
        expect(binaryVersion).toBe("0.1.0");
        expect(minVersion).toBe("1.0.0");
        return compatible;
      },
    });

    try {
      const response = await bridge.send("ping");
      expect(response).toMatchObject({ success: true, source: "compatible", command: "ping" });
      expect(mismatchCalls).toBe(1);
    } finally {
      await bridge.shutdown();
    }
  });

  test("pool replaceBinary does not mark the current mismatch bridge as shutting down", async () => {
    const compatible = writeExecutable(
      "pool-compatible.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [] }) + "\\n");
    } else if (req.command === "version") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, version: "2.0.0" }) + "\\n");
    } else {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, source: "compatible", command: req.command }) + "\\n");
    }
  }
});
`,
    );
    const stale = writeExecutable(
      "pool-stale.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [] }) + "\\n");
    } else if (req.command === "version") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, version: "0.1.0" }) + "\\n");
    } else {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, source: "stale", command: req.command }) + "\\n");
    }
  }
});
`,
    );

    let pool: BridgePool;
    pool = new BridgePool(stale, {
      timeoutMs: 5_000,
      maxRestarts: 0,
      minVersion: "1.0.0",
      onVersionMismatch: async () => pool.replaceBinary(compatible),
    });

    try {
      const bridge = pool.getBridge(workDir);
      const response = await bridge.send("ping");
      expect(response).toMatchObject({ success: true, source: "compatible", command: "ping" });
    } finally {
      await pool.shutdown();
    }
  });

  test("stderr tail buffers split chunks as logical lines", () => {
    const bridge = new BinaryBridge("/fake/aft", workDir, { maxRestarts: 0 });
    const testBridge = bridge as unknown as {
      onStderrData(data: string): void;
      flushStderrBuffer(): void;
      stderrTail: string[];
    };

    testBridge.onStderrData("first half");
    expect(testBridge.stderrTail).toEqual([]);
    testBridge.onStderrData(" second half\nnext");
    expect(testBridge.stderrTail).toEqual(["[aft] first half second half"]);
    testBridge.flushStderrBuffer();
    expect(testBridge.stderrTail).toEqual(["[aft] first half second half", "[aft] next"]);
  });

  test("configureWarningClients evicts entries after delivery and clears on shutdown", async () => {
    const delivered: unknown[] = [];
    const bridge = new BinaryBridge("/fake/aft", workDir, {
      onConfigureWarnings: (context) => {
        delivered.push(context.client);
      },
    });
    const testBridge = bridge as unknown as {
      configureWarningClients: Map<string, unknown>;
      handleConfigureWarningsFrame(frame: Record<string, unknown>): Promise<void>;
      shutdown(): Promise<void>;
    };
    testBridge.configureWarningClients.set("s1", { name: "client-1" });
    testBridge.configureWarningClients.set("s2", { name: "client-2" });
    testBridge.configureWarningClients.set("s3", { name: "client-3" });

    for (const session_id of ["s1", "s2", "s3"]) {
      await testBridge.handleConfigureWarningsFrame({
        type: "configure_warnings",
        session_id,
        warnings: [{ code: "large_repo", message: session_id }],
      });
    }

    expect(delivered).toHaveLength(3);
    expect(testBridge.configureWarningClients.size).toBe(0);

    testBridge.configureWarningClients.set("stale", { name: "stale-client" });
    await testBridge.shutdown();
    expect(testBridge.configureWarningClients.size).toBe(0);
  });

  test("constructor logger overrides active singleton (Oracle F9 — D2 deferral)", () => {
    type LogCall = { level: string; message: string; meta?: LogMeta };
    const makeLogger = (label: string): Logger & { calls: LogCall[] } => {
      const calls: LogCall[] = [];
      const logger = {
        log(message: string, meta?: LogMeta) {
          calls.push({ level: `log:${label}`, message, meta });
        },
        warn(message: string, meta?: LogMeta) {
          calls.push({ level: `warn:${label}`, message, meta });
        },
        error(message: string, meta?: LogMeta) {
          calls.push({ level: `error:${label}`, message, meta });
        },
        getLogFilePath: () => undefined,
        calls,
      };
      return logger;
    };
    const custom = makeLogger("custom");
    const active = makeLogger("active");
    setActiveLogger(active);

    const bridge = new BinaryBridge("/fake/aft", workDir, {
      maxRestarts: 0,
      logger: custom,
    });
    const testBridge = bridge as unknown as {
      logVia(message: string, meta?: LogMeta): void;
      warnVia(message: string, meta?: LogMeta): void;
      errorVia(message: string, meta?: LogMeta): void;
    };

    testBridge.logVia("hello", { kind: "log" });
    testBridge.warnVia("careful", { kind: "warn" });
    testBridge.errorVia("boom", { kind: "error" });

    // Custom logger receives all three; active singleton receives none.
    expect(custom.calls.map((c) => c.level)).toEqual(["log:custom", "warn:custom", "error:custom"]);
    expect(active.calls).toEqual([]);
  });
});
