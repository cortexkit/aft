import type { ChildProcess } from "node:child_process";

import { fail } from "./errors.js";

export interface TaskState {
  id: string;
  status: string;
  status_reason?: string;
  pgid?: number;
}

export interface TaskProbe {
  states: () => Promise<TaskState[]>;
  cancel?: (id: string) => Promise<void>;
}

export interface TrackedProcess {
  id: string;
  pid: number;
  pgid: number;
  source: "aft" | "fallback" | "host" | "mock";
  terminal: boolean;
  exit_code?: number | null;
  signal?: NodeJS.Signals | null;
}

export interface QuiescenceEvidence {
  started_at: string;
  completed_at: string;
  tasks: TaskState[];
  processes: TrackedProcess[];
  stopped: boolean;
}

const TERMINAL_TASK_STATES = new Set([
  "cancelled",
  "completed",
  "failed",
  "killed",
  "stopped",
  "timed_out",
]);

export function processGroupAlive(pgid: number): boolean {
  if (!Number.isInteger(pgid) || pgid <= 0) return false;
  try {
    process.kill(-pgid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

export class ProcessObserver {
  readonly scenario: string;
  readonly processes: TrackedProcess[] = [];
  readonly evidence: QuiescenceEvidence[] = [];
  readonly taskProbe?: TaskProbe;

  constructor(scenario: string, taskProbe?: TaskProbe) {
    this.scenario = scenario;
    this.taskProbe = taskProbe;
  }

  trackChild(id: string, child: ChildProcess, source: TrackedProcess["source"]): TrackedProcess {
    if (!child.pid) throw new Error(`cannot track ${id}: child has no pid`);
    const tracked: TrackedProcess = {
      id,
      pid: child.pid,
      pgid: child.pid,
      source,
      terminal: child.exitCode !== null || child.signalCode !== null,
    };
    child.once("exit", (code, signal) => {
      tracked.terminal = true;
      tracked.exit_code = code;
      tracked.signal = signal;
    });
    this.processes.push(tracked);
    return tracked;
  }

  trackPgid(id: string, pgid: number, source: TrackedProcess["source"]): TrackedProcess {
    const tracked: TrackedProcess = { id, pid: pgid, pgid, source, terminal: false };
    this.processes.push(tracked);
    return tracked;
  }

  async cancelSurvivors(): Promise<void> {
    const tasks = (await this.taskProbe?.states()) ?? [];
    for (const task of tasks) {
      if (!TERMINAL_TASK_STATES.has(task.status) && this.taskProbe?.cancel) {
        await this.taskProbe.cancel(task.id);
      }
    }
    for (const processRecord of this.processes) {
      if (!processGroupAlive(processRecord.pgid)) {
        processRecord.terminal = true;
        continue;
      }
      try {
        process.kill(-processRecord.pgid, "SIGTERM");
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
      }
    }
  }

  async waitForTermination(timeoutMs = 30_000): Promise<QuiescenceEvidence> {
    const started = new Date().toISOString();
    const deadline = Date.now() + timeoutMs;
    let tasks: TaskState[] = [];
    for (;;) {
      tasks = (await this.taskProbe?.states()) ?? [];
      const tasksStopped = tasks.every((task) => TERMINAL_TASK_STATES.has(task.status));
      const processesStopped = this.processes.every((processRecord) => {
        const stopped = !processGroupAlive(processRecord.pgid);
        if (stopped) processRecord.terminal = true;
        return stopped;
      });
      if (tasksStopped && processesStopped) {
        const evidence = {
          started_at: started,
          completed_at: new Date().toISOString(),
          tasks,
          processes: structuredClone(this.processes),
          stopped: true,
        };
        this.evidence.push(evidence);
        return evidence;
      }
      if (Date.now() >= deadline) break;
      await Bun.sleep(100);
    }
    const evidence = {
      started_at: started,
      completed_at: new Date().toISOString(),
      tasks,
      processes: structuredClone(this.processes),
      stopped: false,
    };
    this.evidence.push(evidence);
    return evidence;
  }

  async cleanupAndConfirm(timeoutMs = 30_000): Promise<QuiescenceEvidence> {
    await this.cancelSurvivors();
    let evidence = await this.waitForTermination(Math.min(timeoutMs, 5_000));
    if (evidence.stopped) return evidence;
    for (const processRecord of this.processes) {
      if (!processGroupAlive(processRecord.pgid)) continue;
      try {
        process.kill(-processRecord.pgid, "SIGKILL");
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
      }
    }
    evidence = await this.waitForTermination(Math.max(0, timeoutMs - 5_000));
    return evidence;
  }

  assertStopped(evidence: QuiescenceEvidence): void {
    if (!evidence.stopped) {
      fail(
        "disk_effect_observation_incomplete",
        this.scenario,
        { scenario: this.scenario, tasks: evidence.tasks, processes: evidence.processes },
        true,
      );
    }
  }
}
