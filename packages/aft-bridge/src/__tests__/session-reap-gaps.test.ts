import { expect, test } from "bun:test";
import { SubcTransportPool } from "../subc-transport.js";
import { observeFreshSessionStart } from "../transport.js";
import { TEST_PROJECT_ROOT } from "./subc-test-roots.js";

function makePool(
  reply: Record<string, unknown> | ((name: string) => Record<string, unknown>) = {
    success: true,
    text: "read",
  },
) {
  return new SubcTransportPool({
    connectionFile: "/fake",
    harness: "pi",
    connect: async () => ({
      routeOpen: async () => ({ channel: 1, epoch: 1 }),
      request: async (_route, body) => ({
        structuredContent: {
          text: "",
          ...(typeof reply === "function" ? reply((body as { name: string }).name) : reply),
        },
      }),
      subscribe: () => {
        throw new Error("no background handler is registered");
      },
      closeRouteChannel: async () => undefined,
      close: () => undefined,
    }),
  });
}

test("control: an observed task-free session is reaped after the idle TTL", async () => {
  const pool = makePool();
  try {
    observeFreshSessionStart(pool, TEST_PROJECT_ROOT, "fresh", "new");
    await pool.toolCall(TEST_PROJECT_ROOT, { sessionID: "fresh" }, "read");
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(1);
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(0);
  } finally {
    await pool.shutdown();
  }
});

test("a delivered foreground bash result does not pin an observed idle session", async () => {
  const pool = makePool({
    success: true,
    text: "ready",
    task_id: "bash-0123456789abcdef",
    status: "completed",
    exit_code: 0,
    output_preview: "ready\n",
  });
  try {
    observeFreshSessionStart(pool, TEST_PROJECT_ROOT, "fresh", "new");
    await pool.toolCall(TEST_PROJECT_ROOT, { sessionID: "fresh" }, "bash", {
      command: "echo ready",
      background: false,
    });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(0);
  } finally {
    await pool.shutdown();
  }
});

test("an empty inline PowerShell result does not pin an observed idle session", async () => {
  const pool = makePool({
    success: true,
    task_id: "task-powershell",
    status: "completed",
    output_preview: "",
  });
  try {
    observeFreshSessionStart(pool, TEST_PROJECT_ROOT, "fresh", "new");
    await pool.toolCall(TEST_PROJECT_ROOT, { sessionID: "fresh" }, "powershell", {
      command: "Write-Output ready",
    });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(0);
  } finally {
    await pool.shutdown();
  }
});

test("a terminal bash_watch reply settles a tracked background task for idle reaping", async () => {
  const task_id = "task-watched";
  const pool = makePool((name) => ({
    success: true,
    task_id,
    status: name === "bash" ? "running" : "completed",
  }));
  try {
    observeFreshSessionStart(pool, TEST_PROJECT_ROOT, "fresh", "new");
    const call = (name: string, args: Record<string, unknown>) =>
      pool.toolCall(TEST_PROJECT_ROOT, { sessionID: "fresh" }, name, args);
    await call("bash", { command: "echo ready", background: true });
    await call("bash_status", { task_id });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(1);
    await call("bash_watch", { task_id });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(0);
  } finally {
    await pool.shutdown();
  }
});

test.each([
  ["a detached foreground command", false, "running"],
  ["a fast background launch", true, "completed"],
] as const)("%s stays pinned without inline output until its completion is ACKed", async (_label, background, status) => {
  const task_id = "task-undelivered";
  const pool = makePool((name) => {
    if (name === "bash") return { success: true, task_id, status };
    if (name === "bash_drain_completions")
      return {
        success: true,
        bg_completions: [{ task_id, status: "completed" }],
        pending_matches: [],
      };
    return { success: true, acked_task_ids: [task_id] };
  });
  try {
    observeFreshSessionStart(pool, TEST_PROJECT_ROOT, "fresh", "new");
    const call = (name: string, args: Record<string, unknown> = {}) =>
      pool.toolCall(TEST_PROJECT_ROOT, { sessionID: "fresh" }, name, args);
    await call("bash", { command: "echo ready", background });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(1);
    await call("bash_drain_completions");
    await call("bash_ack_completions", { task_ids: [task_id] });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(0);
  } finally {
    await pool.shutdown();
  }
});
