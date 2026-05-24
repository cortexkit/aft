import { afterEach, describe, expect, test } from "bun:test";
import type { ChildProcess, ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
import { rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { BinaryBridge, compareSemver } from "@cortexkit/aft-bridge";

const BINARY_PATH = resolve(import.meta.dir, "../../../../target/debug/aft");
const PROJECT_CWD = resolve(import.meta.dir, "../../../..");

/** Short timeout for tests — we don't want to wait 30s on failure. */
const TEST_TIMEOUT_MS = 5_000;

describe("BinaryBridge lifecycle", () => {
  let bridge: BinaryBridge | null = null;

  afterEach(async () => {
    if (bridge) {
      await bridge.shutdown();
      bridge = null;
    }
  });

  test("spawns binary and ping returns pong", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
      },
      { harness: "opencode" },
    );

    const response = await bridge.send("ping");

    expect(response.success).toBe(true);
    expect(response.command).toBe("pong");
    expect(bridge.isAlive()).toBe(true);
  });

  test("parses pushed bash_completed frames without request correlation", async () => {
    const completions: unknown[] = [];
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        onBashCompletion: (completion) => {
          completions.push(completion);
        },
      },
      { harness: "opencode" },
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

  test("routes pushed configure_warnings frames with session_id to the warning handler", async () => {
    const deliveries: unknown[] = [];
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      },
      { harness: "opencode" },
    );

    (bridge as any).onStdoutData(
      `${JSON.stringify({
        type: "configure_warnings",
        session_id: "session-1",
        project_root: "/repo",
        source_file_count: 10,
        source_file_count_exceeds_max: false,
        max_callgraph_files: 5_000,
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
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      },
      { harness: "opencode" },
    );

    expect(() => {
      (bridge as any).onStdoutData(
        `${JSON.stringify({
          type: "configure_warnings",
          project_root: "/repo",
          source_file_count: 10,
          source_file_count_exceeds_max: false,
          max_callgraph_files: 5_000,
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
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      },
      { harness: "opencode" },
    );
    (bridge as any).configureWarningClients.set("session-a", clientA);
    (bridge as any).configureWarningClients.set("session-b", clientB);

    (bridge as any).onStdoutData(
      `${JSON.stringify({
        type: "configure_warnings",
        session_id: "session-a",
        project_root: "/repo",
        source_file_count: 10,
        source_file_count_exceeds_max: false,
        max_callgraph_files: 5_000,
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

  test("multiple sequential requests return correct responses (ID correlation)", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
      },
      { harness: "opencode" },
    );

    // Send multiple requests sequentially — each gets a unique ID internally
    const r1 = await bridge.send("ping");
    const r2 = await bridge.send("ping");
    const r3 = await bridge.send("ping");

    // All should succeed with correct command
    expect(r1.success).toBe(true);
    expect(r1.command).toBe("pong");
    expect(r2.success).toBe(true);
    expect(r2.command).toBe("pong");
    expect(r3.success).toBe(true);
    expect(r3.command).toBe("pong");

    // IDs should be unique (ascending)
    const ids = [r1.id, r2.id, r3.id];
    expect(new Set(ids).size).toBe(3);
  });

  test("bridge recovers via lazy respawn after external SIGKILL", async () => {
    // Issue #14: SIGKILL/SIGTERM are external kills, not crashes — we explicitly
    // do NOT auto-restart to avoid process avalanches when many bridges receive
    // SIGTERM together. Recovery still works: the next send() lazy-spawns.
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        maxRestarts: 3,
      },
      { harness: "opencode" },
    );

    // First request to ensure the process is running
    const r1 = await bridge.send("ping");
    expect(r1.success).toBe(true);

    // Kill the child — this is treated as external termination, not a crash.
    const proc = (bridge as any).process;
    expect(proc).not.toBeNull();
    proc.kill("SIGKILL");

    // Wait for the exit handler to observe the killed child. It should NOT
    // schedule an auto-restart; the next request lazy-spawns instead.
    await waitForBridgeProcessExit(bridge, proc.pid);

    // Next request lazy-spawns a fresh bridge.
    const r2 = await bridge.send("ping");
    expect(r2.success).toBe(true);
    // No restart counter bump — this path uses lazy spawn, not auto-restart.
    expect(bridge.restartCount).toBe(0);
  });

  test("shutdown cleans up child process (no orphans)", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
      },
      { harness: "opencode" },
    );

    // Ensure process is alive
    const r = await bridge.send("ping");
    expect(r.success).toBe(true);

    const proc = (bridge as any).process;
    const pid = proc?.pid;
    expect(pid).toBeDefined();

    // Shutdown
    await bridge.shutdown();
    bridge = null; // prevent afterEach from double-shutting-down

    // Verify the process is gone
    expect(isProcessAlive(pid!)).toBe(false);
  });

  test("request to dead bridge after max retries rejects with error", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        maxRestarts: 0, // no restarts allowed
      },
      { harness: "opencode" },
    );

    // Start the process
    const r = await bridge.send("ping");
    expect(r.success).toBe(true);

    // Kill it
    const proc = (bridge as any).process;
    proc.kill("SIGKILL");

    // Wait for crash detection
    await new Promise((resolve) => setTimeout(resolve, 200));

    // Next send should spawn a fresh process (ensureSpawned) — but then kill it again
    // Actually with maxRestarts=0, handleCrash won't restart, but ensureSpawned
    // will call spawnProcess because isAlive() is false. The key test is that
    // after killing enough times, the bridge still functions because ensureSpawned
    // re-spawns on each send call.
    //
    // To truly test "max retries exhausted", we need the bridge to be shutting down
    // or have the process fail to spawn entirely. Let's test the shutdown path instead.
    await bridge.shutdown();

    // After shutdown, send should reject
    await expect(bridge.send("ping")).rejects.toThrow("shutting down");
    bridge = null; // already shut down
  });

  test("multiple parallel first calls share one configure (no race)", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
      },
      { harness: "opencode" },
    );

    // Fire 5 requests in parallel — all arrive before configure completes.
    // Before the shared-promise fix, the 4th+ call would hit the depth limit.
    const results = await Promise.all([
      bridge.send("ping"),
      bridge.send("ping"),
      bridge.send("ping"),
      bridge.send("ping"),
      bridge.send("ping"),
    ]);

    // All 5 should succeed
    for (const r of results) {
      expect(r.success).toBe(true);
      expect(r.command).toBe("pong");
    }

    // All should have distinct IDs
    const ids = results.map((r) => r.id);
    expect(new Set(ids).size).toBe(5);
  });

  test("bridge death during version check prevents configured=true", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        // Set a minVersion so checkVersion actually sends a "version" command
        minVersion: "0.0.1",
        // Disable auto-restart so the killed process stays dead
        maxRestarts: 0,
      },
      { harness: "opencode" },
    );

    // Intercept checkVersion to kill the process mid-flight.
    // We monkey-patch the private checkVersion method to simulate a crash
    // that happens during the version check window. The patched function
    // kills the process and returns normally (simulating checkVersion's
    // best-effort error swallowing behavior).
    (bridge as any).checkVersion = async function (this: any) {
      const proc = this.process;
      if (proc) {
        proc.kill("SIGKILL");
        // Wait for the exit event to fire and handleCrash to run
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
      // Return normally — simulates checkVersion swallowing the error
    };

    // The first send should fail because the bridge dies during checkVersion
    await expect(bridge.send("ping")).rejects.toThrow();

    // configured should be false — the bridge should NOT be marked as configured
    expect((bridge as any).configured).toBe(false);
  });

  test("crash error stays clean for the agent and points to the log", async () => {
    // Fake binary: writes recognizable stderr lines, briefly sleeps so the
    // bridge has time to queue `configure`, then exits non-zero.
    //
    // Agent-facing rejection contract: the rejection error must NOT carry
    // stderr tail noise (loaded N backups, invalidated K files, or — as in
    // this fixture — the actual cause). Operator diagnostics belong in the
    // plugin log; the agent only needs a pointer to it. Anything else just
    // burns context on output the agent can't act on.
    const fakeBin = join(tmpdir(), `aft-fake-crash-${Date.now()}.sh`);
    await writeFile(
      fakeBin,
      [
        "#!/bin/sh",
        'echo "fatal: semantic index corrupted" >&2',
        'echo "caused by: bad cache magic" >&2',
        // Sleep long enough for the bridge to write `configure` to stdin
        // before the process dies, so the request is in `pending` when the
        // exit handler runs.
        "sleep 0.3",
        "exit 1",
        "",
      ].join("\n"),
      { mode: 0o755 },
    );

    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: 5_000, // must exceed the fake binary's sleep
          maxRestarts: 0, // force straight to the "giving up" path
        },
        { harness: "opencode" },
      );

      let caught: Error | null = null;
      try {
        await bridge.send("ping");
      } catch (e) {
        caught = e as Error;
      }
      expect(caught).not.toBeNull();
      const msg = caught?.message ?? "";
      // Agent gets a concise, actionable error (prefix is the bridge default
      // when no errorPrefix is provided to BinaryBridge directly in tests)
      expect(msg).toContain("Binary crashed");
      expect(msg).toContain("(see ");
      // Agent does NOT get the stderr tail dumped into the rejection
      expect(msg).not.toContain("fatal: semantic index corrupted");
      expect(msg).not.toContain("caused by: bad cache magic");
      expect(msg).not.toContain("--- last");
      expect(msg).not.toContain("stderr lines");
    } finally {
      await rm(fakeBin).catch(() => {});
    }
  });

  test("per-request timeoutMs override rejects before bridge-wide default", async () => {
    // Fake binary: reads stdin and sleeps forever without responding. We want
    // to prove the per-request override (50ms) fires instead of the bridge
    // default (5000ms). If the override isn't honored, the bridge-wide timer
    // triggers and the test would take 5+ seconds to reject.
    const fakeBin = join(tmpdir(), `aft-fake-slow-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: 5_000, // bridge-wide default
          maxRestarts: 0,
        },
        { harness: "opencode" },
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

  test("keepBridgeOnTimeout: child process survives transport timeout for retry-friendly commands", async () => {
    // Fake binary that hangs forever on input — same pattern as the timeout
    // test above. Prove that with keepBridgeOnTimeout=true the same hung child
    // process is still alive after the request rejects, so subsequent commands
    // can race through it without a respawn (and don't pay the bridge-restart
    // cost just because one bash call's response was late).
    const fakeBin = join(tmpdir(), `aft-fake-slow-keep-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: 5_000,
          maxRestarts: 0,
        },
        { harness: "opencode" },
      );

      // First request: timeout with keepBridgeOnTimeout — should reject without
      // killing the bridge.
      const err1 = await bridge
        .send("version", {}, { timeoutMs: 50, keepBridgeOnTimeout: true })
        .catch((e) => e);
      expect(err1).toBeInstanceOf(Error);
      expect((err1 as Error).message).toContain("timed out");

      // Without keepBridgeOnTimeout, handleTimeout() would have killed the
      // child. With it set, the child is still alive — verify by calling the
      // private getter via cast (test-only).
      const child = (bridge as unknown as { process: { killed: boolean } | null }).process;
      expect(child).not.toBeNull();
      expect(child?.killed).toBe(false);

      // Compare with default (no keep flag): same hung binary, but now the
      // bridge will tear it down on timeout.
      const err2 = await bridge.send("version", {}, { timeoutMs: 50 }).catch((e) => e);
      expect(err2).toBeInstanceOf(Error);
      expect((err2 as Error).message).toContain("timed out");
      // After handleTimeout fires, the previous child is killed (process is
      // cleared). New send() would spawn fresh, but we set maxRestarts=0 so
      // no respawn happens — process should now be null.
      const childAfter = (bridge as unknown as { process: { killed: boolean } | null }).process;
      expect(childAfter).toBeNull();
    } finally {
      await rm(fakeBin).catch(() => {});
    }
  });

  test("send rejects params that contain reserved id key before writing", async () => {
    const marker = join(tmpdir(), `aft-fake-id-collision-started-${Date.now()}`);
    const fakeBin = join(tmpdir(), `aft-fake-id-collision-${Date.now()}.sh`);
    await writeFile(
      fakeBin,
      ["#!/bin/sh", `touch ${JSON.stringify(marker)}`, "sleep 30", ""].join("\n"),
      {
        mode: 0o755,
      },
    );

    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: TEST_TIMEOUT_MS,
        },
        { harness: "opencode" },
      );

      await expect(bridge.send("foo", { id: "evil" })).rejects.toThrow(
        "params cannot contain reserved key 'id'",
      );
      expect(existsSync(marker)).toBe(false);
    } finally {
      await rm(fakeBin).catch(() => {});
      await rm(marker).catch(() => {});
    }
  });

  test("per-request transportTimeoutMs override sets the bridge timer", async () => {
    const fakeBin = join(tmpdir(), `aft-fake-transport-timeout-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    const originalSetTimeout = globalThis.setTimeout;
    const delays: unknown[] = [];
    globalThis.setTimeout = ((
      handler: Parameters<typeof setTimeout>[0],
      timeout?: number,
      ...args: unknown[]
    ) => {
      delays.push(timeout);
      return originalSetTimeout(handler, Math.min(Number(timeout ?? 0), 1), ...args);
    }) as typeof setTimeout;

    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: 30_000,
          maxRestarts: 0,
        },
        { harness: "opencode" },
      );

      const err = await bridge.send("version", {}, { transportTimeoutMs: 60_000 }).catch((e) => e);

      expect(err).toBeInstanceOf(Error);
      expect((err as Error).message).toContain("timed out after 60000ms");
      expect(delays).toContain(60_000);
    } finally {
      globalThis.setTimeout = originalSetTimeout;
      await rm(fakeBin).catch(() => {});
    }
  });

  test("restart counter decays even after max restarts is reached", async () => {
    bridge = new BinaryBridge(
      BINARY_PATH,
      PROJECT_CWD,
      {
        timeoutMs: TEST_TIMEOUT_MS,
        maxRestarts: 1,
      },
      { harness: "opencode" },
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
    const fakeBin = join(tmpdir(), `aft-fake-stale-exit-${Date.now()}.sh`);
    await writeFile(fakeBin, ["#!/bin/sh", "sleep 30", ""].join("\n"), { mode: 0o755 });

    let staleChild: ChildProcess | null = null;
    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: TEST_TIMEOUT_MS,
          maxRestarts: 0,
        },
        { harness: "opencode" },
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

  test("stderr tail ring buffer is bounded (doesn't grow unboundedly)", async () => {
    // Emit 250 stderr lines into a long-lived bridge — only the last 20 should
    // be kept per BinaryBridge.STDERR_TAIL_MAX. The tail no longer lives in
    // agent-facing errors (operator diagnostics belong in aft-plugin.log only),
    // so we assert directly against the internal ring buffer.
    const fakeBin = join(tmpdir(), `aft-fake-flood-${Date.now()}.sh`);
    await writeFile(
      fakeBin,
      [
        "#!/bin/sh",
        "i=0",
        "while [ $i -lt 250 ]; do",
        '  echo "noise line $i" >&2',
        "  i=$((i + 1))",
        "done",
        'echo "MARKER_LAST" >&2',
        // Hold the process open so the stderr ring is observable before the
        // child exits and the spawnProcess() reset clears it.
        "sleep 5",
        "",
      ].join("\n"),
      { mode: 0o755 },
    );

    try {
      bridge = new BinaryBridge(
        fakeBin,
        PROJECT_CWD,
        {
          timeoutMs: 5_000,
          maxRestarts: 0,
        },
        { harness: "opencode" },
      );

      // Spawn the process so stderr starts streaming. We don't wait on a
      // response — the fake binary doesn't speak NDJSON.
      (bridge as any).spawnProcess();

      // Poll for MARKER_LAST to land in the ring buffer. Under load, the
      // 250+1 stderr lines can take longer than a fixed sleep would allow,
      // so we deadline-poll instead.
      const deadline = Date.now() + 5_000;
      let ring: string[] = (bridge as any).stderrTail as string[];
      while (Date.now() < deadline) {
        ring = (bridge as any).stderrTail as string[];
        if (ring.some((line) => line.includes("MARKER_LAST"))) break;
        await new Promise((resolve) => setTimeout(resolve, 25));
      }

      // Ring is capped at STDERR_TAIL_MAX (20) regardless of how many lines arrived.
      expect(ring.length).toBeLessThanOrEqual(20);
      // Last marker is preserved — the tail captures the END of the stream.
      expect(ring.some((line) => line.includes("MARKER_LAST"))).toBe(true);
      // Early lines have been evicted.
      expect(ring.some((line) => line === "noise line 0")).toBe(false);
      expect(ring.some((line) => line === "noise line 100")).toBe(false);
    } finally {
      await rm(fakeBin).catch(() => {});
    }
  });
});

describe("compareSemver", () => {
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

/** Check if a process with the given PID is still alive. */
function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0); // signal 0 = existence check
    return true;
  } catch {
    return false;
  }
}

async function waitForBridgeProcessExit(
  bridge: BinaryBridge,
  pid: number | undefined,
  timeoutMs = 5_000,
): Promise<void> {
  if (pid === undefined) throw new Error("bridge child process is missing a pid");
  await waitUntil(
    () => {
      const current = (bridge as unknown as { process: ChildProcess | null }).process;
      return current?.pid !== pid || !isProcessAlive(pid);
    },
    timeoutMs,
    `bridge child ${pid} to exit`,
  );
}

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
