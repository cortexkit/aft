/**
 * Bridge-level tests for Pi.
 *
 * Mirrors packages/opencode-plugin/src/__tests__/bridge.test.ts. Both plugins
 * share the same bridge design (per-op timeout, SIGKILL recovery, etc) so we
 * keep coverage mirrored to catch regressions in either package.
 */

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import type { ChildProcess, ChildProcessWithoutNullStreams } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { BinaryBridge, compareSemver } from "@cortexkit/aft-bridge";

let bridge: BinaryBridge | null = null;
let projectRoot: string;
let canaryDataHome: string;
let canarySnapshot: string;

beforeAll(() => {
  canaryDataHome = mkdtempSync(join(tmpdir(), "aft-pi-shared-storage-canary-"));
  const sharedStorage = join(canaryDataHome, "cortexkit", "aft");
  mkdirSync(sharedStorage, { recursive: true });
  writeFileSync(join(sharedStorage, "sentinel"), "must remain unchanged\n");
  canarySnapshot = snapshotFiles(sharedStorage);
});

beforeEach(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-pi-bridge-project-"));
});

afterEach(async () => {
  if (bridge) {
    await bridge.shutdown();
    bridge = null;
  }
  rmSync(projectRoot, { recursive: true, force: true });
});

afterAll(() => {
  expect(snapshotFiles(join(canaryDataHome, "cortexkit", "aft"))).toBe(canarySnapshot);
  rmSync(canaryDataHome, { recursive: true, force: true });
});

function isolatedBridgeOptions<T extends object>(options: T) {
  const childEnv = (options as { childEnv?: Record<string, string | undefined> }).childEnv;
  return {
    ...options,
    childEnv: {
      ...childEnv,
      AFT_STORAGE_DIR: join(projectRoot, ".aft-storage"),
      AFT_CACHE_DIR: join(projectRoot, ".aft-cache"),
      XDG_DATA_HOME: canaryDataHome,
    },
  };
}

function snapshotFiles(root: string): string {
  const files: Array<[string, string, number]> = [];
  const walk = (directory: string) => {
    for (const entry of readdirSync(directory, { withFileTypes: true }).sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) {
        walk(path);
      } else {
        files.push([
          relative(root, path),
          readFileSync(path).toString("base64"),
          statSync(path).mtimeMs,
        ]);
      }
    }
  };
  walk(root);
  return JSON.stringify(files);
}

describe("Pi BinaryBridge", () => {
  test("parses pushed bash_completed frames without request correlation", async () => {
    const completions: unknown[] = [];
    bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      projectRoot,
      isolatedBridgeOptions({
        timeoutMs: 5_000,
        onBashCompletion: (completion) => {
          completions.push(completion);
        },
      }),
      { harness: "pi" },
    );

    (bridge as any).onStdoutData(
      `${JSON.stringify({
        type: "bash_completed",
        task_id: "task-1",
        session_id: "s1",
        status: "completed",
        exit_code: 0,
        command: "echo done",
      })}\n`,
    );

    expect(completions).toEqual([
      {
        type: "bash_completed",
        task_id: "task-1",
        session_id: "s1",
        status: "completed",
        exit_code: 0,
        command: "echo done",
      },
    ]);
  });

  test("parses final stdout frame without trailing newline when stream ends", async () => {
    const completions: unknown[] = [];
    bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      projectRoot,
      isolatedBridgeOptions({
        timeoutMs: 5_000,
        onBashCompletion: (completion) => {
          completions.push(completion);
        },
      }),
      { harness: "pi" },
    );

    (bridge as any).onStdoutData(
      JSON.stringify({
        type: "bash_completed",
        task_id: "task-final",
        session_id: "s1",
        status: "completed",
        exit_code: 0,
        command: "echo done",
      }),
    );
    (bridge as any).flushStdoutBuffer();

    expect(completions).toHaveLength(1);
    expect((completions[0] as { task_id?: string }).task_id).toBe("task-final");
  });

  test("routes pushed configure_warnings frames with session_id to the warning handler", async () => {
    const deliveries: unknown[] = [];
    bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      projectRoot,
      isolatedBridgeOptions({
        timeoutMs: 5_000,
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      }),
      { harness: "pi" },
    );

    (bridge as any).onStdoutData(
      `${JSON.stringify({
        type: "configure_warnings",
        session_id: "session-1",
        project_root: "/repo",
        warnings: [
          {
            kind: "formatter_not_installed",
            language: "typescript",
            tool: "biome",
            hint: "Install biome.",
          },
        ],
      })}\n`,
    );

    expect(deliveries).toHaveLength(1);
    expect(deliveries[0]).toEqual({
      projectRoot: "/repo",
      sessionId: "session-1",
      client: undefined,
      warnings: [
        {
          kind: "formatter_not_installed",
          language: "typescript",
          tool: "biome",
          hint: "Install biome.",
        },
      ],
    });
  });

  test("handles pushed configure_warnings frames with missing session_id gracefully", async () => {
    const deliveries: unknown[] = [];
    bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      projectRoot,
      isolatedBridgeOptions({
        timeoutMs: 5_000,
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      }),
      { harness: "pi" },
    );

    expect(() => {
      (bridge as any).onStdoutData(
        `${JSON.stringify({
          type: "configure_warnings",
          project_root: "/repo",
          warnings: [
            {
              kind: "formatter_not_installed",
              language: "typescript",
              tool: "biome",
              hint: "Install biome.",
            },
          ],
        })}\n`,
      );
    }).not.toThrow();

    expect(deliveries).toHaveLength(1);
    expect(deliveries[0]).toEqual({
      projectRoot: "/repo",
      sessionId: null,
      client: undefined,
      warnings: [
        {
          kind: "formatter_not_installed",
          language: "typescript",
          tool: "biome",
          hint: "Install biome.",
        },
      ],
    });
  });

  test("uses the session_id to pick the matching configure warning client", async () => {
    const deliveries: unknown[] = [];
    const clientA = { name: "client-a" };
    const clientB = { name: "client-b" };
    bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      projectRoot,
      isolatedBridgeOptions({
        timeoutMs: 5_000,
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      }),
      { harness: "pi" },
    );
    (bridge as any).configureWarningClients.set("session-a", clientA);
    (bridge as any).configureWarningClients.set("session-b", clientB);

    (bridge as any).onStdoutData(
      `${JSON.stringify({
        type: "configure_warnings",
        session_id: "session-a",
        project_root: "/repo",
        warnings: [
          {
            kind: "formatter_not_installed",
            language: "typescript",
            tool: "biome",
            hint: "Install biome.",
          },
        ],
      })}\n`,
    );

    expect(deliveries).toHaveLength(1);
    expect((deliveries[0] as { client?: unknown }).client).toBe(clientA);
  });

  test("per-request timeoutMs override rejects before bridge-wide default", async () => {
    // Fake binary: reads stdin and sleeps forever without responding. We want
    // to prove the per-request override (50ms) fires instead of the bridge
    // default (5000ms). If the override isn't honored, the bridge-wide timer
    // triggers and the test would take 5+ seconds to reject.
    const fakeBin = join(tmpdir(), `aft-pi-fake-slow-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    try {
      bridge = new BinaryBridge(
        fakeBin,
        projectRoot,
        isolatedBridgeOptions({
          timeoutMs: 5_000, // bridge-wide default
          maxRestarts: 0,
        }),
        { harness: "pi" },
      );

      const start = Date.now();
      // Use "version" to skip the auto-configure path (configure/version are
      // the two commands that bypass it). Pass a tight 50ms override — should
      // reject in ~50ms, not 5000ms. If the override weren't honored, the
      // bridge-wide 5s timer would trigger instead and `elapsed` would be ~5s.
      const err = await bridge.send("version", {}, { timeoutMs: 50 }).catch((e) => e);
      const elapsed = Date.now() - start;

      expect(err).toBeInstanceOf(Error);
      expect((err as Error).message).toContain("timed out after 50ms");
      // Allow generous slack so CI flakes don't fail this — but must be well
      // under the 5s bridge default to prove the override took effect.
      expect(elapsed).toBeLessThan(2_000);
    } finally {
      await rm(fakeBin).catch(() => {});
    }
  });

  test("restart counter decays even after max restarts is reached", async () => {
    bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      projectRoot,
      isolatedBridgeOptions({
        timeoutMs: 5_000,
        maxRestarts: 1,
      }),
      { harness: "pi" },
    );
    const originalResetMs = (BinaryBridge as any).RESTART_RESET_MS;

    try {
      (BinaryBridge as any).RESTART_RESET_MS = 20;
      (bridge as any)._restartCount = 1;

      (bridge as any).handleCrash();

      expect(bridge.restartCount).toBe(1);
      await waitUntil(() => bridge?.restartCount === 0, 500, "restart counter to decay");
      expect(bridge.restartCount).toBe(0);
    } finally {
      (BinaryBridge as any).RESTART_RESET_MS = originalResetMs;
    }
  });

  test("stale exit from replaced child is ignored", async () => {
    const fakeBin = join(tmpdir(), `aft-pi-fake-stale-exit-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    let staleChild: ChildProcess | null = null;
    try {
      bridge = new BinaryBridge(
        fakeBin,
        projectRoot,
        isolatedBridgeOptions({
          timeoutMs: 5_000,
          maxRestarts: 0,
        }),
        { harness: "pi" },
      );

      (bridge as any).spawnProcess();
      staleChild = (bridge as any).process as ChildProcessWithoutNullStreams;
      (bridge as any).spawnProcess();
      const activeChild = (bridge as any).process as ChildProcessWithoutNullStreams;
      (bridge as any).configured = true;

      staleChild.emit("exit", 1, null);

      expect((bridge as any).process).toBe(activeChild);
      expect((bridge as any).configured).toBe(true);
    } finally {
      staleChild?.kill("SIGKILL");
      await rm(fakeBin).catch(() => {});
    }
  });

  test("rejects params with reserved id before writing to the bridge", async () => {
    const fakeBin = join(tmpdir(), `aft-pi-fake-id-collision-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    try {
      bridge = new BinaryBridge(
        fakeBin,
        projectRoot,
        isolatedBridgeOptions({
          timeoutMs: 5_000,
          maxRestarts: 0,
        }),
        { harness: "pi" },
      );

      await expect(bridge.send("read", { id: "caller-id", filePath: "x.ts" })).rejects.toThrow(
        "params cannot contain reserved key 'id'",
      );
      expect(bridge.isAlive()).toBe(false);
    } finally {
      await rm(fakeBin).catch(() => {});
    }
  });
});

describe("Pi compareSemver", () => {
  test("orders semver pre-release identifiers per spec", () => {
    const ordered = [
      "1.0.0-alpha",
      "1.0.0-alpha.1",
      "1.0.0-alpha.beta",
      "1.0.0-beta",
      "1.0.0-beta.2",
      "1.0.0-beta.11",
      "1.0.0-rc.1",
      "1.0.0",
    ];

    for (let i = 0; i < ordered.length - 1; i++) {
      expect(compareSemver(ordered[i], ordered[i + 1])).toBeLessThan(0);
      expect(compareSemver(ordered[i + 1], ordered[i])).toBeGreaterThan(0);
    }
    expect(compareSemver("1.0.0-beta.1", "1.0.0")).toBeLessThan(0);
    expect(compareSemver("1.2.0", "1.1.99-rc.1")).toBeGreaterThan(0);
    expect(compareSemver("1.0.0-alpha.1", "1.0.0-alpha.1")).toBe(0);
  });
});

async function waitUntil(
  predicate: () => boolean | Promise<boolean>,
  timeoutMs: number,
  description: string,
): Promise<void> {
  const started = Date.now();
  while (!(await predicate())) {
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for ${description}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}
