import type { ChildProcess } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";

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

export interface TaskStatusWaitEvidence {
  expected_status: string;
  elapsed_ms: number;
  task: TaskState;
  observed: TaskState[];
}

/** Wait until the task registry proves a task reached the requested state. */
export async function waitForTaskStatus(
  probe: TaskProbe,
  status: string,
  timeoutMs: number,
): Promise<TaskStatusWaitEvidence> {
  const startedAt = Date.now();
  const deadline = startedAt + timeoutMs;
  let observed: TaskState[] = [];

  for (;;) {
    observed = await probe.states();
    const task = observed.find((candidate) => candidate.status === status);
    if (task) {
      return {
        expected_status: status,
        elapsed_ms: Date.now() - startedAt,
        task,
        observed,
      };
    }
    if (Date.now() >= deadline) {
      throw new Error(
        `task probe did not observe status ${JSON.stringify(status)} within ${timeoutMs}ms; observed ${JSON.stringify(observed)}`,
      );
    }
    await Bun.sleep(Math.min(25, deadline - Date.now()));
  }
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

/**
 * Whether any member of a process group is still a process that can run.
 *
 * A killed process stays in the process table as a zombie until its parent
 * reaps it, and a background task whose parent died is reparented onto the
 * process the container started with, which never waits for it. Signalling
 * cannot tell those apart — `kill(-pgid, 0)` succeeds for a group of zombies
 * exactly as it does for a group doing work — so a task left behind by a host
 * the harness had to kill would read as still writing forever.
 *
 * Reading the kernel's own view of each member settles it: a zombie has
 * already exited and can touch nothing. `undefined` means this system does not
 * publish process state under `/proc`, and the caller keeps the signal answer.
 */
export function processGroupRunning(pgid: number, procRoot = "/proc"): boolean | undefined {
  let entries: string[];
  try {
    entries = readdirSync(procRoot);
  } catch {
    return undefined;
  }
  let members = 0;
  for (const entry of entries) {
    if (!/^\d+$/.test(entry)) continue;
    let stat: string;
    try {
      stat = readFileSync(`${procRoot}/${entry}/stat`, "utf8");
    } catch {
      continue;
    }
    // The command name sits in parentheses and may contain spaces, so the
    // fields after it are read from the last closing parenthesis onwards:
    // state, then parent pid, then process group.
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
    if (Number(fields[2]) !== pgid) continue;
    members += 1;
    if (fields[0] !== "Z") return true;
  }
  // A group the kernel reports as present but whose members cannot be listed
  // is treated as running, so an unreadable process is never mistaken for a
  // stopped one.
  return members === 0 ? true : false;
}

export function processGroupAlive(pgid: number): boolean {
  if (!Number.isInteger(pgid) || pgid <= 0) return false;
  try {
    process.kill(-pgid, 0);
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
  return processGroupRunning(pgid) ?? true;
}

/**
 * Whether a task could still be writing.
 *
 * A task row's status is written by the AFT daemon, and the daemon does not
 * outlive the host: a task deliberately left running past the end of a run —
 * the ones a scenario marks `outlives_result` — keeps the status it had when
 * the daemon went away, however thoroughly the harness has since killed it.
 * Waiting for that field to say `cancelled` waits forever. The process group
 * is the thing the check is actually about, and the harness can read it
 * directly, so a row whose group is gone counts as stopped whatever its
 * status says. A task with no recorded group cannot be shown to have stopped,
 * so it counts as running.
 */
function taskStopped(task: TaskState): boolean {
  if (TERMINAL_TASK_STATES.has(task.status)) return true;
  return task.pgid !== undefined && !processGroupAlive(task.pgid);
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
    for (const pgid of this.#groupsToStop(tasks)) {
      if (!processGroupAlive(pgid)) continue;
      try {
        process.kill(-pgid, "SIGTERM");
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
      }
    }
    for (const processRecord of this.processes) {
      if (!processGroupAlive(processRecord.pgid)) processRecord.terminal = true;
    }
  }

  /**
   * Every process group this run is responsible for ending.
   *
   * A background task runs in a group of its own so it can outlive the call
   * that started it, which also means killing the host's group does not reach
   * it. Cancelling through the task probe only works while something is alive
   * to act on the cancellation: a host that had to be killed takes the daemon
   * with it, and the task is left running with nobody to stop it. Naming the
   * task groups here is what makes the harness finish what its own kill
   * started, instead of reporting a tree it left running.
   */
  #groupsToStop(tasks: readonly TaskState[]): number[] {
    const groups = this.processes.map((processRecord) => processRecord.pgid);
    for (const task of tasks) {
      if (task.pgid !== undefined && !groups.includes(task.pgid)) groups.push(task.pgid);
    }
    return groups;
  }

  async waitForTermination(timeoutMs = 30_000): Promise<QuiescenceEvidence> {
    const started = new Date().toISOString();
    const deadline = Date.now() + timeoutMs;
    let tasks: TaskState[] = [];
    for (;;) {
      tasks = (await this.taskProbe?.states()) ?? [];
      const tasksStopped = tasks.every((task) => taskStopped(task));
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
    for (const pgid of this.#groupsToStop(evidence.tasks)) {
      if (!processGroupAlive(pgid)) continue;
      try {
        process.kill(-pgid, "SIGKILL");
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
