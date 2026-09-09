import { lstat, readdir, readlink } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join } from "node:path";

import { fail, HarnessError } from "./errors.js";
import type {
  DiskEffectDeclaration,
  DiskEffectPhase,
  ThreeStatePathExpectation,
  ToolCallPlan,
} from "./types.js";
import { fixtureRelativePath, pathInside, sha256File } from "./util.js";

export type PathState = { state: "absent" } | { state: "present"; sha256: string };
export type RootSnapshot = ReadonlyMap<string, PathState>;

export interface DiskCheckpoint {
  name: string;
  phase: DiskEffectPhase | "baseline" | "terminal";
  captured_at: string;
  paths: Record<string, PathState>;
}

export interface DiskSweepFailure {
  code: "undeclared_disk_effect";
  scenario: string;
  originating_call: string;
  task_ids: string[];
  process_ids: number[];
  checkpoint: string;
  phase: string;
  path: string;
  before: PathState;
  after: PathState;
}

interface ActiveCall {
  id: string;
  tool: string;
  allowed: Map<string, DiskEffectPhase>;
  outlivesResult: boolean;
  terminal: boolean;
  taskIds: string[];
  processIds: number[];
  baseline: RootSnapshot;
  previous: RootSnapshot;
}

const ABSENT: PathState = { state: "absent" };

function stateEquals(left: PathState, right: PathState): boolean {
  return (
    left.state === right.state &&
    (left.state === "absent" || left.sha256 === (right as PathState & { sha256: string }).sha256)
  );
}

function serializeSnapshot(snapshot: RootSnapshot): Record<string, PathState> {
  return Object.fromEntries(
    [...snapshot.entries()].sort(([left], [right]) => left.localeCompare(right)),
  );
}

function changedPaths(before: RootSnapshot, after: RootSnapshot): string[] {
  const paths = new Set([...before.keys(), ...after.keys()]);
  return [...paths]
    .filter((path) => !stateEquals(before.get(path) ?? ABSENT, after.get(path) ?? ABSENT))
    .sort();
}

async function hashSymlink(path: string): Promise<string> {
  return createHash("sha256")
    .update(`symlink:${await readlink(path)}`)
    .digest("hex");
}

export async function capturePathState(root: string, path: string): Promise<PathState> {
  const relative = fixtureRelativePath(root, path);
  const absolute = pathInside(root, relative);
  try {
    const entry = await lstat(absolute);
    if (entry.isSymbolicLink()) return { state: "present", sha256: await hashSymlink(absolute) };
    if (entry.isFile()) return { state: "present", sha256: await sha256File(absolute) };
    if (entry.isDirectory()) {
      return { state: "present", sha256: createHash("sha256").update("directory").digest("hex") };
    }
    return {
      state: "present",
      sha256: createHash("sha256").update(`special:${entry.mode}:${entry.size}`).digest("hex"),
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return ABSENT;
    throw error;
  }
}

export async function snapshotPaths(
  root: string,
  paths: readonly string[],
): Promise<Record<string, PathState>> {
  return Object.fromEntries(
    await Promise.all(
      paths.map(async (path) => {
        const relative = fixtureRelativePath(root, path);
        return [relative, await capturePathState(root, relative)] as const;
      }),
    ),
  );
}

export async function snapshotRoot(root: string): Promise<RootSnapshot> {
  const snapshot = new Map<string, PathState>();
  async function walk(directory: string, prefix: string): Promise<void> {
    const entries = await readdir(directory, { withFileTypes: true });
    entries.sort((left, right) => left.name.localeCompare(right.name));
    for (const entry of entries) {
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (relative === ".git" || relative.startsWith(".git/")) continue;
      if (relative === ".harness-state" || relative.startsWith(".harness-state/")) continue;
      const absolute = join(directory, entry.name);
      if (entry.isDirectory()) {
        await walk(absolute, relative);
      } else if (entry.isSymbolicLink()) {
        snapshot.set(relative, { state: "present", sha256: await hashSymlink(absolute) });
      } else if (entry.isFile()) {
        snapshot.set(relative, { state: "present", sha256: await sha256File(absolute) });
      } else {
        const stat = await lstat(absolute);
        snapshot.set(relative, {
          state: "present",
          sha256: createHash("sha256").update(`special:${stat.mode}:${stat.size}`).digest("hex"),
        });
      }
    }
  }
  await walk(root, "");
  return snapshot;
}

function expectedIntermediate(
  expectation: ThreeStatePathExpectation,
  pre: PathState,
  intermediate: PathState,
  preStates: Readonly<Record<string, PathState>>,
): boolean {
  switch (expectation.transition) {
    case "create":
      return pre.state === "absent" && intermediate.state === "present";
    case "delete":
    case "move_source":
      return pre.state === "present" && intermediate.state === "absent";
    case "edit":
      return (
        pre.state === "present" &&
        intermediate.state === "present" &&
        pre.sha256 !== intermediate.sha256
      );
    case "move_destination": {
      const source = expectation.paired_path ? preStates[expectation.paired_path] : undefined;
      return (
        pre.state === "absent" &&
        intermediate.state === "present" &&
        source?.state === "present" &&
        intermediate.sha256 === source.sha256
      );
    }
    case "move_overwrite_destination":
      return (
        pre.state === "present" &&
        intermediate.state === "present" &&
        pre.sha256 !== intermediate.sha256
      );
  }
}

export function assertThreeStateRestore(
  before: Readonly<Record<string, PathState>>,
  intermediate: Readonly<Record<string, PathState>>,
  restored: Readonly<Record<string, PathState>>,
  expectations: readonly ThreeStatePathExpectation[],
): void {
  for (const expectation of expectations) {
    const path = expectation.path;
    if (
      (expectation.transition === "create" ||
        expectation.transition === "move_overwrite_destination") &&
      !expectation.expected_intermediate_sha256
    ) {
      fail(
        "fixture_invalid",
        `expected intermediate bytes are required for ${path}`,
        { path, transition: expectation.transition },
        true,
      );
    }
    const pre = before[path] ?? ABSENT;
    const middle = intermediate[path] ?? ABSENT;
    const after = restored[path] ?? ABSENT;
    if (stateEquals(pre, middle)) {
      fail("no_effect_observed", path, { path, before: pre, intermediate: middle }, true);
    }
    if (!expectedIntermediate(expectation, pre, middle, before)) {
      fail(
        "fixture_invalid",
        `unexpected intermediate transition for ${path}`,
        { path, transition: expectation.transition, before: pre, intermediate: middle },
        true,
      );
    }
    if (
      expectation.expected_intermediate_sha256 &&
      (middle.state !== "present" || middle.sha256 !== expectation.expected_intermediate_sha256)
    ) {
      fail(
        "fixture_invalid",
        `intermediate bytes do not match for ${path}`,
        { path, expected: expectation.expected_intermediate_sha256, intermediate: middle },
        true,
      );
    }
    if (!stateEquals(pre, after)) {
      fail(
        "fixture_invalid",
        `restore did not recover ${path}`,
        { path, before: pre, restored: after },
        true,
      );
    }
  }
}

function normalizeEffects(root: string, call: ToolCallPlan): Map<string, DiskEffectPhase> {
  const allowed = new Map<string, DiskEffectPhase>();
  for (const declaration of call.disk_effects ?? []) {
    const effect: DiskEffectDeclaration =
      typeof declaration === "string" ? { path: declaration, phase: "any" } : declaration;
    const path = fixtureRelativePath(root, effect.path);
    if (allowed.has(path)) throw new Error(`duplicate disk effect declaration: ${call.id}:${path}`);
    allowed.set(path, effect.phase ?? "any");
  }
  return allowed;
}

function phaseAllows(declared: DiskEffectPhase | undefined, observed: string): boolean {
  if (declared === "any") return true;
  if (observed === "result") return declared === "result";
  if (observed === "cancellation") return declared === "cancellation" || declared === "post_result";
  return declared === "post_result";
}

export class ThreeStateRecorder {
  readonly root: string;
  readonly expectations: readonly ThreeStatePathExpectation[];
  before?: Record<string, PathState>;
  intermediate?: Record<string, PathState>;
  restored?: Record<string, PathState>;

  constructor(root: string, expectations: readonly ThreeStatePathExpectation[]) {
    this.root = root;
    this.expectations = expectations.map((expectation) => ({
      ...expectation,
      path: fixtureRelativePath(root, expectation.path),
      paired_path: expectation.paired_path
        ? fixtureRelativePath(root, expectation.paired_path)
        : undefined,
    }));
  }

  async capture(phase: "before" | "intermediate" | "restored"): Promise<void> {
    if (phase === "intermediate" && !this.before)
      throw new Error("three-state intermediate preceded before");
    if (phase === "restored" && !this.intermediate)
      throw new Error("three-state restore preceded intermediate");
    if (this[phase]) throw new Error(`three-state phase captured twice: ${phase}`);
    this[phase] = await snapshotPaths(
      this.root,
      this.expectations.map((expectation) => expectation.path),
    );
  }

  assertComplete(): void {
    if (!this.before || !this.intermediate || !this.restored) {
      fail(
        "fixture_invalid",
        "three-state restore evidence is incomplete",
        {
          before: Boolean(this.before),
          intermediate: Boolean(this.intermediate),
          restored: Boolean(this.restored),
        },
        true,
      );
    }
    assertThreeStateRestore(this.before, this.intermediate, this.restored, this.expectations);
  }
}

export class DiskStateObserver {
  readonly checkpoints: DiskCheckpoint[] = [];
  readonly failures: DiskSweepFailure[] = [];
  readonly root: string;
  readonly scenario: string;
  #active = new Map<string, ActiveCall>();

  constructor(root: string, scenario: string) {
    this.root = root;
    this.scenario = scenario;
  }

  async beginCall(call: ToolCallPlan): Promise<void> {
    if (this.#active.has(call.id)) throw new Error(`duplicate originating call id: ${call.id}`);
    const writer = [...this.#active.values()].find(
      (active) => active.outlivesResult && !active.terminal,
    );
    if (writer && (call.disk_effects?.length ?? 0) > 0) {
      fail(
        "scenario_invalid",
        `overlapping fixture writers ${writer.id} and ${call.id}`,
        { first_call: writer.id, second_call: call.id },
        true,
      );
    }
    const baseline = await snapshotRoot(this.root);
    this.#active.set(call.id, {
      id: call.id,
      tool: call.name,
      allowed: normalizeEffects(this.root, call),
      outlivesResult: call.outlives_result === true,
      terminal: false,
      taskIds: [...(call.task_ids ?? [])],
      processIds: [...(call.process_ids ?? [])],
      baseline,
      previous: baseline,
    });
    this.recordCheckpoint(`${call.id}:baseline`, "baseline", baseline);
  }

  async checkpointCall(callId: string, checkpoint: string, phase: DiskEffectPhase): Promise<void> {
    const active = this.#active.get(callId);
    if (!active) throw new Error(`unknown originating call: ${callId}`);
    const current = await snapshotRoot(this.root);
    this.recordCheckpoint(`${callId}:${checkpoint}`, phase, current);
    for (const path of changedPaths(active.previous, current)) {
      const declared = active.allowed.get(path);
      if (phaseAllows(declared, phase)) continue;
      this.failures.push({
        code: "undeclared_disk_effect",
        scenario: this.scenario,
        originating_call: active.id,
        task_ids: active.taskIds,
        process_ids: active.processIds,
        checkpoint,
        phase,
        path,
        before: active.previous.get(path) ?? ABSENT,
        after: current.get(path) ?? ABSENT,
      });
    }
    active.previous = current;
    if (phase === "result" && !active.outlivesResult) active.terminal = true;
    if (phase === "cancellation") active.terminal = true;
    this.throwFirstFailure();
  }

  markTerminal(callId: string, taskIds: string[] = [], processIds: number[] = []): void {
    const active = this.#active.get(callId);
    if (!active) throw new Error(`unknown originating call: ${callId}`);
    active.taskIds.push(...taskIds.filter((id) => !active.taskIds.includes(id)));
    active.processIds.push(...processIds.filter((id) => !active.processIds.includes(id)));
    active.terminal = true;
  }

  async finalize(writersStopped: boolean): Promise<void> {
    const final = await snapshotRoot(this.root);
    this.recordCheckpoint("scenario:final-after-termination", "terminal", final);
    for (const active of this.#active.values()) {
      if (active.terminal && !active.outlivesResult) continue;
      for (const path of changedPaths(active.previous, final)) {
        const declared = active.allowed.get(path);
        if (phaseAllows(declared, "post_result")) continue;
        this.failures.push({
          code: "undeclared_disk_effect",
          scenario: this.scenario,
          originating_call: active.id,
          task_ids: active.taskIds,
          process_ids: active.processIds,
          checkpoint: "final-after-termination",
          phase: "post_result",
          path,
          before: active.previous.get(path) ?? ABSENT,
          after: final.get(path) ?? ABSENT,
        });
      }
      active.previous = final;
    }
    this.throwFirstFailure();
    const nonterminal = [...this.#active.values()]
      .filter((call) => !call.terminal)
      .map((call) => call.id);
    if (!writersStopped || nonterminal.length > 0) {
      fail(
        "disk_effect_observation_incomplete",
        this.scenario,
        {
          scenario: this.scenario,
          nonterminal_calls: nonterminal,
          writers_stopped: writersStopped,
        },
        true,
      );
    }
  }

  recordCheckpoint(name: string, phase: DiskCheckpoint["phase"], snapshot: RootSnapshot): void {
    this.checkpoints.push({
      name,
      phase,
      captured_at: new Date().toISOString(),
      paths: serializeSnapshot(snapshot),
    });
  }

  throwFirstFailure(): void {
    const failure = this.failures[0];
    if (!failure) return;
    throw new HarnessError(
      "undeclared_disk_effect",
      failure.path,
      failure as unknown as Record<string, unknown>,
      true,
    );
  }
}
