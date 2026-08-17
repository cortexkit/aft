/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { ChildProcess } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setActiveLogger } from "../active-logger.js";
import { BinaryBridge, isBridgeTransportTimeout } from "../bridge.js";
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
  test("hashline registration is immutable and session-scoped across concurrent calls", async () => {
    const requestsPath = join(workDir, "requests.ndjson");
    const script = writeExecutable(
      "hashline-registration.js",
      `#!/usr/bin/env node
const { appendFileSync } = require("node:fs");
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    appendFileSync(${JSON.stringify(requestsPath)}, JSON.stringify(req) + "\\n");
    process.stdout.write(JSON.stringify({ id: req.id, success: true, text: "ok", warnings: [] }) + "\\n");
  }
});
`,
    );
    const logs: Array<{ message: string; meta?: LogMeta }> = [];
    const logger: Logger = {
      log: (message, meta) => logs.push({ message, meta }),
      warn: () => {},
      error: () => {},
    };
    const pool = new BridgePool(script, {
      idleTimeoutMs: Infinity,
      timeoutMs: 5_000,
      maxRestarts: 0,
      logger,
    });
    pool.setConfigureOverride("harness", "opencode");
    pool.setConfigureOverride("edit_slot_survives", true);

    try {
      await Promise.all([
        pool.toolCall(workDir, { sessionID: "session-a" }, "read", { path: "sample.ts" }),
        pool.toolCall(workDir, { sessionID: "session-b" }, "read", { path: "sample.ts" }),
      ]);

      const requests = readFileSync(requestsPath, "utf8")
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line) as Record<string, unknown>);
      const configure = requests.find((request) => request.command === "configure");
      expect(configure).toMatchObject({ edit_slot_survives: true });
      const toolCalls = requests.filter((request) => request.command === "tool_call");
      expect(toolCalls).toHaveLength(2);
      expect(
        toolCalls
          .map((request) => ({
            session_id: request.session_id,
            edit_slot_survives: request.edit_slot_survives,
          }))
          .sort((left, right) => String(left.session_id).localeCompare(String(right.session_id))),
      ).toEqual([
        { session_id: "session-a", edit_slot_survives: true },
        { session_id: "session-b", edit_slot_survives: true },
      ]);

      const carrierLogs = logs.filter(({ message }) =>
        message.includes(
          "hashline registration carrier transport=ndjson phase=tool_call edit_slot_survives=true",
        ),
      );
      expect(carrierLogs).toHaveLength(2);
      expect(carrierLogs.map(({ meta }) => meta?.sessionId).sort()).toEqual([
        "session-a",
        "session-b",
      ]);
      expect(() => pool.setConfigureOverride("edit_slot_survives", false)).toThrow(
        "edit_slot_survives is write-once",
      );
    } finally {
      await pool.shutdown();
    }
  });

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

  test("respawns when child stdin closes before process exit", async () => {
    const script = writeExecutable(
      "closed-stdin.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
setInterval(() => {}, 1_000);
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [], pid: process.pid }) + "\\n");
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { process: ChildProcess | null };

    try {
      const first = await bridge.send("ping");
      const firstPid = first.pid;
      const staleChild = testBridge.process;
      if (!staleChild?.stdin) throw new Error("bridge child stdin is unavailable");

      const closed = new Promise<void>((resolve) => staleChild.stdin?.once("close", resolve));
      staleChild.stdin.destroy();
      await closed;

      expect(staleChild.exitCode).toBeNull();
      const second = await bridge.send("ping");
      expect(second.pid).not.toBe(firstPid);
    } finally {
      await bridge.shutdown();
    }
  });

  test("invalidates the child after an asynchronous stdin write failure", async () => {
    const script = writeExecutable(
      "write-error.js",
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
    process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [], pid: process.pid }) + "\\n");
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { process: ChildProcess | null };

    try {
      const first = await bridge.send("ping");
      const staleChild = testBridge.process;
      if (!staleChild?.stdin) throw new Error("bridge child stdin is unavailable");

      staleChild.stdin.write = ((_chunk: unknown, callback?: (error?: Error | null) => void) => {
        queueMicrotask(() => callback?.(new Error("simulated EPIPE")));
        return false;
      }) as typeof staleChild.stdin.write;

      await expect(bridge.send("mutate")).rejects.toThrow("simulated EPIPE");
      const second = await bridge.send("ping");
      expect(second.pid).not.toBe(first.pid);
    } finally {
      await bridge.shutdown();
    }
  });

  test("ignores stale child stdout end after replacement owns the buffer", async () => {
    const script = writeExecutable(
      "stale-stdout.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
setInterval(() => {}, 1_000);
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [], pid: process.pid }) + "\\n");
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as {
      process: ChildProcess | null;
      stdoutBuffer: string;
    };

    try {
      await bridge.send("ping");
      const staleChild = testBridge.process;
      if (!staleChild?.stdin) throw new Error("bridge child stdin is unavailable");

      const closed = new Promise<void>((resolve) => staleChild.stdin?.once("close", resolve));
      staleChild.stdin.destroy();
      await closed;
      await bridge.send("ping");

      testBridge.stdoutBuffer = '{"id":"replacement"';
      staleChild.stdout?.emit("end");
      expect(testBridge.stdoutBuffer).toBe('{"id":"replacement"');
    } finally {
      await bridge.shutdown();
    }
  });

  test("does not carry configure completion across child generations", async () => {
    const script = writeExecutable(
      "configure-generation.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
let configured = false;
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") configured = true;
    process.stdout.write(JSON.stringify({
      id: req.id,
      success: true,
      warnings: [],
      configured,
      pid: process.pid,
    }) + "\\n");
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as {
      process: ChildProcess | null;
      deliverConfigureWarnings: () => Promise<void>;
    };
    let releaseWarnings: (() => void) | undefined;
    let warningsEntered: (() => void) | undefined;
    const warningGate = new Promise<void>((resolve) => {
      releaseWarnings = resolve;
    });
    const entered = new Promise<void>((resolve) => {
      warningsEntered = resolve;
    });
    testBridge.deliverConfigureWarnings = async () => {
      warningsEntered?.();
      await warningGate;
    };

    try {
      const first = bridge.send("ping");
      await entered;
      const staleChild = testBridge.process;
      if (!staleChild?.stdin) throw new Error("bridge child stdin is unavailable");

      const closed = new Promise<void>((resolve) => staleChild.stdin?.once("close", resolve));
      staleChild.stdin.destroy();
      await closed;
      const second = bridge.send("ping");
      releaseWarnings?.();
      await Promise.allSettled([first, second]);

      const third = await bridge.send("ping");
      expect(third.configured).toBe(true);
    } finally {
      releaseWarnings?.();
      await bridge.shutdown();
    }
  });

  test("invalidates the child when stdin write throws synchronously", async () => {
    const script = writeExecutable(
      "sync-write-error.js",
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
    process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [], pid: process.pid }) + "\\n");
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as {
      process: ChildProcess | null;
      pending: Map<string, unknown>;
    };

    try {
      const first = await bridge.send("ping");
      const staleChild = testBridge.process;
      if (!staleChild?.stdin) throw new Error("bridge child stdin is unavailable");
      staleChild.stdin.write = (() => {
        throw new Error("synchronous write failure");
      }) as typeof staleChild.stdin.write;

      await expect(bridge.send("mutate")).rejects.toThrow("synchronous write failure");
      expect(testBridge.pending.size).toBe(0);
      const second = await bridge.send("ping");
      expect(second.pid).not.toBe(first.pid);
    } finally {
      await bridge.shutdown();
    }
  });

  test("single timeout with child stdout activity keeps bridge warm and sibling alive", async () => {
    const script = writeExecutable(
      "alive-starvation.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "sibling") {
      setTimeout(() => writeFrame({ type: "status_changed", snapshot: { source: "sibling" } }), 5);
      setTimeout(() => writeFrame({ id: req.id, success: true, command: req.command }), 80);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { configured: boolean };

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      const timedOutResult = bridge.send("slow", {}, { timeoutMs: 40 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      const siblingResult = bridge.send("sibling", {}, { timeoutMs: 500 }).then(
        (response) => String(response.command),
        (err) => String(err instanceof Error ? err.message : err),
      );

      const [timedOut, sibling] = await Promise.all([timedOutResult, siblingResult]);

      expect(timedOut).toContain("bridge kept warm");
      expect(sibling).toBe("sibling");
      expect(bridge.isAlive()).toBe(true);
      expect(testBridge.configured).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("single silent timeout below hang threshold keeps bridge warm and sibling alive", async () => {
    const script = writeExecutable(
      "single-silent-timeout.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "sibling") {
      setTimeout(() => writeFrame({ id: req.id, success: true, command: req.command }), 80);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { configured: boolean };

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      const timedOutResult = bridge.send("slow", {}, { timeoutMs: 30 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      const siblingResult = bridge.send("sibling", {}, { timeoutMs: 500 }).then(
        (response) => String(response.command),
        (err) => String(err instanceof Error ? err.message : err),
      );

      const [timedOut, sibling] = await Promise.all([timedOutResult, siblingResult]);

      expect(timedOut).toContain("bridge kept warm");
      expect(sibling).toBe("sibling");
      expect(bridge.isAlive()).toBe(true);
      expect(testBridge.configured).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("repeated silent timeouts escalate to bridge kill and abort siblings", async () => {
    const script = writeExecutable(
      "silent-hang.js",
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
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { configured: boolean };

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      const first = await bridge.send("first", {}, { timeoutMs: 20 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      expect(first).toContain("bridge kept warm");

      const secondResult = bridge.send("second", {}, { timeoutMs: 20 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      const siblingResult = bridge.send("sibling", {}, { timeoutMs: 1_000 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );

      const [second, sibling] = (await Promise.race([
        Promise.all([secondResult, siblingResult]),
        new Promise<[string, string]>((resolve) =>
          setTimeout(() => resolve(["pending", "pending"]), 200),
        ),
      ])) as [string, string];

      expect(second).toMatch(/timed out|aborted/);
      expect(sibling).toContain("sibling timeout");
      expect(bridge.isAlive()).toBe(false);
      expect(testBridge.configured).toBe(false);
    } finally {
      await bridge.shutdown();
    }
  });

  test("custom hangThreshold delays bridge kill until threshold silent timeouts", async () => {
    const script = writeExecutable(
      "hang-threshold-five.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "sibling") {
      setTimeout(() => writeFrame({ id: req.id, success: true, command: req.command }), 80);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, {
      timeoutMs: 1_000,
      hangThreshold: 5,
      maxRestarts: 0,
    });

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      for (let i = 0; i < 4; i++) {
        const msg = await bridge.send(`silent-${i}`, {}, { timeoutMs: 25 }).then(
          () => "resolved",
          (err) => String(err instanceof Error ? err.message : err),
        );
        expect(msg).toContain("bridge kept warm");
        expect(bridge.isAlive()).toBe(true);
      }

      const siblingResult = bridge.send("sibling", {}, { timeoutMs: 500 }).then(
        (response) => String(response.command),
        (err) => String(err instanceof Error ? err.message : err),
      );
      expect(await siblingResult).toBe("sibling");
      expect(bridge.isAlive()).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("passive status polls never kill the bridge, even past the hang threshold (#117)", async () => {
    // Mock a busy bridge: it answers `sibling` but NEVER answers `status`
    // (status queues behind real work and times out). With the DEFAULT hang
    // threshold of 2, two un-flagged timeouts would normally SIGKILL the bridge
    // — issue #117, where sidebar status polls killed the bridge mid-Edit and
    // aborted the user's queued request. `status` is now bridge-side passive:
    // it must keep the bridge warm no matter how many polls time out, and never
    // need an explicit keepBridgeOnTimeout from the caller.
    const script = writeExecutable(
      "busy-status.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "sibling") {
      setTimeout(() => writeFrame({ id: req.id, success: true, command: req.command }), 30);
    }
    // status: deliberately never answered (simulates a busy bridge).
  }
});
`,
    );
    // Default threshold (2) + small timeout so the test is fast. Passive caps
    // status at min(80, 5000) = 80ms.
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 80, maxRestarts: 0 });

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      // Five consecutive status timeouts — far past the threshold of 2. None
      // may kill the bridge, and the caller passes NO keepBridgeOnTimeout.
      for (let i = 0; i < 5; i++) {
        const msg = await bridge.send("status", { session_id: "s" }).then(
          () => "resolved",
          (err) => String(err instanceof Error ? err.message : err),
        );
        expect(msg).toContain("timed out");
        expect(msg).not.toContain("restarting");
        expect(bridge.isAlive()).toBe(true);
      }

      // The bridge is still the SAME live process and serves real work.
      const sibling = await bridge.send("sibling", {}, { timeoutMs: 500 }).then(
        (r) => String(r.command),
        (err) => String(err instanceof Error ? err.message : err),
      );
      expect(sibling).toBe("sibling");
      expect(bridge.isAlive()).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("custom timeoutMs is used as default per-request transport budget", async () => {
    const script = writeExecutable(
      "slow-ping.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "ping") {
      setTimeout(() => writeFrame({ id: req.id, success: true, command: "ping" }), 120);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 80, maxRestarts: 0 });

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const fastFail = await bridge.send("ping", {}).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      expect(fastFail).toMatch(/timed out after 80ms/);
      expect(bridge.isAlive()).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("successful response between timeouts resets hang escalation", async () => {
    const script = writeExecutable(
      "timeout-reset.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "ok") {
      writeFrame({ id: req.id, success: true, command: req.command });
    } else if (req.command === "sibling") {
      setTimeout(() => writeFrame({ id: req.id, success: true, command: req.command }), 80);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { configured: boolean };

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      const first = await bridge.send("first-timeout", {}, { timeoutMs: 20 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      expect(first).toContain("bridge kept warm");

      const ok = await bridge.send("ok", {}, { timeoutMs: 500 });
      expect(ok).toMatchObject({ success: true, command: "ok" });

      const timedOutResult = bridge.send("second-timeout", {}, { timeoutMs: 30 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      const siblingResult = bridge.send("sibling", {}, { timeoutMs: 500 }).then(
        (response) => String(response.command),
        (err) => String(err instanceof Error ? err.message : err),
      );

      const [timedOut, sibling] = await Promise.all([timedOutResult, siblingResult]);

      expect(timedOut).toContain("bridge kept warm");
      expect(sibling).toBe("sibling");
      expect(bridge.isAlive()).toBe(true);
      expect(testBridge.configured).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("keepBridgeOnTimeout timeout does not advance hang escalation", async () => {
    const script = writeExecutable(
      "keep-timeout.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "sibling") {
      setTimeout(() => writeFrame({ id: req.id, success: true, command: req.command }), 80);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });
    const testBridge = bridge as unknown as { configured: boolean };

    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      const keepResult = await bridge
        .send("keep", {}, { timeoutMs: 20, keepBridgeOnTimeout: true })
        .then(
          () => "resolved",
          (err) => String(err instanceof Error ? err.message : err),
        );
      expect(keepResult).toContain('Request "keep"');
      expect(bridge.isAlive()).toBe(true);

      const timedOutResult = bridge.send("ordinary", {}, { timeoutMs: 30 }).then(
        () => "resolved",
        (err) => String(err instanceof Error ? err.message : err),
      );
      const siblingResult = bridge.send("sibling", {}, { timeoutMs: 500 }).then(
        (response) => String(response.command),
        (err) => String(err instanceof Error ? err.message : err),
      );

      const [timedOut, sibling] = await Promise.all([timedOutResult, siblingResult]);

      expect(timedOut).toContain("bridge kept warm");
      expect(sibling).toBe("sibling");
      expect(bridge.isAlive()).toBe(true);
      expect(testBridge.configured).toBe(true);
    } finally {
      await bridge.shutdown();
    }
  });

  test("keepBridgeOnTimeout rejection is a typed transport-timeout error", async () => {
    // The bash_watch poll loop must distinguish "bridge busy, retry" from a
    // genuine command failure WITHOUT string-matching the message. A keep-warm
    // timeout rejects with BridgeTransportTimeoutError (code transport_timeout);
    // a non-timeout error does not.
    const script = writeExecutable(
      "typed-timeout.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function writeFrame(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      writeFrame({ id: req.id, success: true, warnings: [] });
    } else if (req.command === "boom") {
      writeFrame({ id: req.id, success: false, code: "explode", message: "kaboom" });
    }
    // "slow" gets no response → transport timeout.
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });

      // Keep-warm transport timeout → typed, code transport_timeout.
      const timeoutErr = await bridge
        .send("slow", {}, { timeoutMs: 20, keepBridgeOnTimeout: true })
        .then(
          () => undefined,
          (err) => err,
        );
      expect(isBridgeTransportTimeout(timeoutErr)).toBe(true);
      expect((timeoutErr as { code?: string }).code).toBe("transport_timeout");
      expect(bridge.isAlive()).toBe(true);

      // A genuine command-failure response is NOT a transport timeout — the
      // poll loop must let this propagate, not retry it away.
      const boom = await bridge.send("boom", {}, { keepBridgeOnTimeout: true }).then(
        (response) => response,
        (err) => err,
      );
      expect(isBridgeTransportTimeout(boom)).toBe(false);
    } finally {
      await bridge.shutdown();
    }
  });

  test("caller transport timeout applies to implicit configure and version RPCs", async () => {
    const script = writeExecutable(
      "slow-cold-start.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
function reply(req, body, delay) {
  setTimeout(() => {
    process.stdout.write(JSON.stringify({ id: req.id, ...body }) + "\\n");
  }, delay);
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      reply(req, { success: true, warnings: [] }, 120);
    } else if (req.command === "version") {
      reply(req, { success: true, version: "1.0.0" }, 120);
    } else {
      reply(req, { success: true, command: req.command }, 0);
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, {
      timeoutMs: 50,
      maxRestarts: 0,
      minVersion: "1.0.0",
    });

    try {
      // 5s budget proves the same contract (caller's transportTimeoutMs is
      // used for implicit configure + version RPCs instead of the bridge's
      // default 50ms) while tolerating CI/Mac load. Node startup + 2x 120ms
      // server delays could exceed the previous 1s under load.
      const response = await bridge.send("ping", {}, { transportTimeoutMs: 5_000 });
      expect(response).toMatchObject({ success: true, command: "ping" });
    } finally {
      await bridge.shutdown();
    }
  });

  test("explicit configure success marks bridge configured", async () => {
    const script = writeExecutable(
      "explicit-configure.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
let configureCount = 0;
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      configureCount += 1;
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [] }) + "\\n");
    } else {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, command: req.command, configureCount }) + "\\n");
    }
  }
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 5_000, maxRestarts: 0 });

    try {
      await bridge.send("configure", { project_root: workDir });
      const response = await bridge.send("ping");
      expect(response).toMatchObject({ success: true, command: "ping", configureCount: 1 });
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
