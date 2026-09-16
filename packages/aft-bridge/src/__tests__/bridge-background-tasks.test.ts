import { describe, expect, test } from "bun:test";
import { BinaryBridge } from "../bridge.js";

type PendingForTest = {
  resolve: (value: Record<string, unknown>) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
  command: string;
};

function internals(bridge: BinaryBridge): {
  pending: Map<string, PendingForTest>;
  processStdoutLine(line: string): void;
  onStdoutData(data: string): void;
} {
  return bridge as unknown as {
    pending: Map<string, PendingForTest>;
    processStdoutLine(line: string): void;
    onStdoutData(data: string): void;
  };
}

let nextId = 1;

function enqueuePending(bridge: BinaryBridge, command: string, id = `bg-test-${nextId++}`): string {
  const timer = setTimeout(() => {}, 10_000);
  timer.unref();
  internals(bridge).pending.set(id, {
    command,
    timer,
    resolve: () => {},
    reject: (error) => {
      throw error;
    },
  });
  return id;
}

function deliverResponse(
  bridge: BinaryBridge,
  command: string,
  payload: Record<string, unknown>,
): void {
  const id = enqueuePending(bridge, command);
  internals(bridge).processStdoutLine(JSON.stringify({ id, ...payload }));
}

function completionFrame(
  taskId: string,
  status: "completed" | "failed" | "killed" | "timed_out" | "fate_unknown" = "completed",
): string {
  return JSON.stringify({
    type: "bash_completed",
    task_id: taskId,
    session_id: "session-1",
    status,
    exit_code: status === "completed" ? 0 : null,
    command: "echo done",
  });
}

describe("BinaryBridge background task accounting", () => {
  test("tracks running bash task_id responses and removes them on completion frames", () => {
    const bridge = new BinaryBridge("/tmp/aft-does-not-need-to-exist", process.cwd());

    deliverResponse(bridge, "bash", { success: true, task_id: "bash-running", status: "running" });

    expect(bridge.hasOutstandingBackgroundTasks()).toBe(true);

    internals(bridge).processStdoutLine(completionFrame("bash-running"));

    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
  });

  test("removes tracked tasks on terminal status and kill responses", () => {
    const bridge = new BinaryBridge("/tmp/aft-does-not-need-to-exist", process.cwd());

    deliverResponse(bridge, "bash", { success: true, task_id: "bash-status", status: "running" });
    deliverResponse(bridge, "bash_status", {
      success: true,
      task_id: "bash-status",
      status: "failed",
    });
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);

    deliverResponse(bridge, "bash", { success: true, task_id: "bash-kill", status: "running" });
    deliverResponse(bridge, "bash_kill", {
      success: true,
      task_id: "bash-kill",
      status: "killed",
    });
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);

    deliverResponse(bridge, "bash", { success: true, task_id: "bash-timeout", status: "running" });
    internals(bridge).processStdoutLine(completionFrame("bash-timeout", "timed_out"));
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);

    deliverResponse(bridge, "bash", { success: true, task_id: "bash-orphan", status: "running" });
    internals(bridge).processStdoutLine(completionFrame("bash-orphan", "fate_unknown"));
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
  });

  test("accounts for spawn before resolving when completion arrives in the same stdout chunk", () => {
    const bridge = new BinaryBridge("/tmp/aft-does-not-need-to-exist", process.cwd());
    const id = enqueuePending(bridge, "bash", "spawn-race");

    internals(bridge).onStdoutData(
      `${JSON.stringify({ id, success: true, task_id: "bash-race", status: "running" })}\n${completionFrame("bash-race")}\n`,
    );

    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
  });

  test("accepts camelCase taskId on defensive bash response parsing", () => {
    const bridge = new BinaryBridge("/tmp/aft-does-not-need-to-exist", process.cwd());

    deliverResponse(bridge, "bash", { success: true, taskId: "bash-camel", status: "running" });
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(true);

    internals(bridge).processStdoutLine(completionFrame("bash-camel", "completed"));
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
  });
});

describe("BinaryBridge background task accounting on bridge death", () => {
  function deathInternals(bridge: BinaryBridge): {
    handleTimeout(
      trigger: { requestId: string; tool: string; generation: number },
      triggeringSessionId?: string,
    ): void;
    handleCrash(cause?: Error): void;
    rejectAllPending(error: Error): void;
  } {
    return bridge as unknown as {
      handleTimeout(
        trigger: { requestId: string; tool: string; generation: number },
        triggeringSessionId?: string,
      ): void;
      handleCrash(cause?: Error): void;
      rejectAllPending(error: Error): void;
    };
  }

  test("crash clears outstanding task ids so the bridge does not stay pinned forever", () => {
    // A crash abandons every removal hook (foreground polls rejected, no
    // completion frame from a dead child). Without clearing, the phantom ids
    // pin the bridge against idle eviction permanently — defeating the
    // idle-eviction feature.
    const bridge = new BinaryBridge("/bin/false", { autoRestart: false });
    deliverResponse(bridge, "bash", {
      success: true,
      task_id: "bash-crash1",
      status: "running",
    });
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(true);

    deathInternals(bridge).handleCrash(new Error("boom"));
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
  });

  test("timeout kill clears outstanding task ids", () => {
    const bridge = new BinaryBridge("/bin/false", { autoRestart: false });
    deliverResponse(bridge, "bash", {
      success: true,
      task_id: "bash-timeout1",
      status: "running",
    });
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(true);

    deathInternals(bridge).handleTimeout({ requestId: "1", tool: "bash", generation: 1 });
    expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
  });
});
