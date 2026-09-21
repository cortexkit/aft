import { chmod, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { Subprocess } from "bun";

import { assertThreeStateRestore, DiskStateObserver, type PathState } from "./disk-state.js";
import { HarnessError, type HarnessFailureCode } from "./errors.js";
import {
  assertBashExecutionPathIdentity,
  HOST_FALLBACK_OUTPUT_MARKER,
} from "./permission-plan.js";
import { verifyExecutableProvenance } from "./provenance.js";
import { makeTransportDeadStub, pointTransportDeadStub } from "./transport-stub.js";
import type { ScenarioDefinition, ToolCallPlan } from "./types.js";
import { applyMutatingTestOverride, validateMutatingDeclarations } from "./validation.js";
import { sha256File } from "./util.js";

export interface HarnessControlEvidence {
  control: string;
  expected: string;
  observed: string;
  outcome: "passed";
  unaffected_positive_control?: string;
}

function call(overrides: Partial<ToolCallPlan> = {}): ToolCallPlan {
  return {
    id: "control-call",
    name: "write",
    arguments: { filePath: "effect.txt", content: "effect\n" },
    ...overrides,
  };
}

function scenario(toolCall: ToolCallPlan): ScenarioDefinition {
  return {
    schema_version: 1,
    id: "write/T1/runtime-arm-control",
    tool: "write",
    trajectory: "T1",
    execution: "standalone",
    prompt: "runtime arm control",
    turns: [
      {
        label: "control-call",
        response: { kind: "tool_calls", calls: [toolCall] },
      },
    ],
  };
}

async function expectFailure(
  control: string,
  expected: HarnessFailureCode,
  action: () => Promise<void>,
  unaffectedPositiveControl?: string,
): Promise<HarnessControlEvidence> {
  try {
    await action();
  } catch (error) {
    if (!(error instanceof HarnessError) || error.code !== expected) throw error;
    return {
      control,
      expected,
      observed: `${error.message}${typeof error.details.scenario === "string" ? ` scenario=${error.details.scenario}` : ""}`,
      outcome: "passed",
      unaffected_positive_control: unaffectedPositiveControl,
    };
  }
  throw new Error(`${control} did not fail ${expected}`);
}

async function freshProject(root: string, name: string): Promise<string> {
  const project = join(root, name);
  await rm(project, { recursive: true, force: true });
  await mkdir(project, { recursive: true });
  return project;
}

async function provenanceFixture(
  root: string,
  name: string,
): Promise<{
  repo: string;
  executable: string;
  policy: string;
  sha: string;
}> {
  const repo = await freshProject(root, name);
  Bun.spawnSync(["git", "init", "-q"], { cwd: repo });
  await writeFile(join(repo, "tracked"), "control\n");
  Bun.spawnSync(["git", "add", "tracked"], { cwd: repo });
  const commit = Bun.spawnSync(
    [
      "git",
      "-c",
      "user.name=Harness",
      "-c",
      "user.email=harness@example.invalid",
      "commit",
      "-qm",
      "control",
    ],
    { cwd: repo },
  );
  if (commit.exitCode !== 0) throw new Error(commit.stderr.toString());
  const sha = Bun.spawnSync(["git", "rev-parse", "HEAD"], { cwd: repo }).stdout.toString().trim();
  const executable = join(repo, "aft");
  await writeFile(executable, `#!/bin/sh\necho "aft control (${sha})"\n`);
  await chmod(executable, 0o755);
  const policy = join(repo, "policy.json");
  await writeFile(
    policy,
    `${JSON.stringify({
      schema_version: 1,
      sidecar_name: "build-info.json",
      allowed_sources: ["checkout-build", "same-sha-artifact"],
      allowed_profiles: ["release"],
      required_sidecar_fields: ["git_sha", "build_profile", "sha256", "source"],
    })}\n`,
  );
  return { repo, executable, policy, sha };
}

async function writeBuildInfo(
  fixture: Awaited<ReturnType<typeof provenanceFixture>>,
  overrides: Record<string, unknown> = {},
): Promise<void> {
  await writeFile(
    join(fixture.repo, "build-info.json"),
    `${JSON.stringify({
      git_sha: fixture.sha,
      build_profile: "release",
      sha256: await sha256File(fixture.executable),
      source: "checkout-build",
      ...overrides,
    })}\n`,
  );
}

export async function assertHarnessControlCoverage(
  requiredPath: string,
  evidence: readonly HarnessControlEvidence[],
): Promise<void> {
  const required = JSON.parse(await readFile(requiredPath, "utf8")) as {
    controls?: unknown;
  };
  if (!Array.isArray(required.controls) || required.controls.some((id) => typeof id !== "string")) {
    throw new Error("mutation-controls.json is invalid");
  }
  const observed = new Set(evidence.map((entry) => entry.control));
  for (const id of required.controls) {
    if (!observed.has(id)) throw new Error(`required harness control has no evidence: ${id}`);
  }
}

export async function runHarnessControlSuite(root: string): Promise<HarnessControlEvidence[]> {
  await mkdir(root, { recursive: true });
  const evidence: HarnessControlEvidence[] = [];

  {
    const firstRoot = await freshProject(root, "concurrent-first");
    const secondRoot = await freshProject(root, "concurrent-second");
    const first = new DiskStateObserver(firstRoot, "write/T1/concurrent-first");
    const second = new DiskStateObserver(secondRoot, "write/T1/concurrent-second");
    await Promise.all([
      first.beginCall(call({ disk_effects: ["effect.txt"] })),
      second.beginCall(call({ disk_effects: ["effect.txt"] })),
    ]);
    await Promise.all([
      writeFile(join(firstRoot, "effect.txt"), "first scenario\n"),
      writeFile(join(secondRoot, "effect.txt"), "second scenario\n"),
    ]);
    await Promise.all([
      first.checkpointCall("control-call", "tool-result", "result"),
      second.checkpointCall("control-call", "tool-result", "result"),
    ]);
    evidence.push({
      control: "concurrent-scenario-root-isolation",
      expected: "pass",
      observed: "pass",
      outcome: "passed",
    });
  }

  {
    const writerRoot = await freshProject(root, "cross-root-writer");
    const victimRoot = await freshProject(root, "cross-root-victim");
    const writer = new DiskStateObserver(writerRoot, "bash/T1/cross-root-writer");
    const victim = new DiskStateObserver(victimRoot, "read/T1/cross-root-victim");
    await Promise.all([
      writer.beginCall(call({ name: "bash" })),
      victim.beginCall(call()),
    ]);
    await writeFile(join(victimRoot, "escaped.txt"), "cross-root effect\n");
    await writer.checkpointCall("control-call", "tool-result", "result");
    evidence.push(
      await expectFailure(
        "cross-scenario-root-write-attributed-to-victim-root",
        "undeclared_disk_effect",
        async () => {
          try {
            await victim.checkpointCall("control-call", "tool-result", "result");
          } catch (error) {
            // The effect must be reported under the VICTIM root's scenario id;
            // a writer-labelled or unlabelled report would let two concurrent
            // observers blur, which is what this control exists to refuse.
            if (error instanceof HarnessError && error.details.scenario !== "read/T1/cross-root-victim") {
              throw new Error(
                `cross-root effect attributed to ${String(error.details.scenario)}, expected the victim root read/T1/cross-root-victim`,
              );
            }
            throw error;
          }
        },
        "concurrent-scenario-root-isolation",
      ),
    );
    if (writer.failures.length > 0) {
      throw new Error("the writer observer consumed a sibling root's disk effect");
    }
  }

  {
    const project = await freshProject(root, "ordinary");
    const observer = new DiskStateObserver(project, "write/T1/ordinary-control");
    await observer.beginCall(call());
    await writeFile(join(project, "effect.txt"), "ordinary undeclared effect\n");
    evidence.push(
      await expectFailure("ordinary-undeclared-disk-effect", "undeclared_disk_effect", () =>
        observer.checkpointCall("control-call", "tool-result", "result"),
      ),
    );
  }

  {
    const override = applyMutatingTestOverride(
      new Set(["write"]),
      {
        AFT_OPENCODE2_HARNESS_SELF_TEST: "1",
        AFT_OPENCODE2_TEST_REMOVE_MUTATING_TOOL: "write",
        AFT_OPENCODE2_TEST_NON_MUTATING_EVIDENCE: "fabricated runtime-arm evidence",
      },
      true,
    );
    validateMutatingDeclarations([scenario(call())], override.tools, override.fabricatedEvidence);
    const project = await freshProject(root, "override");
    const observer = new DiskStateObserver(project, "write/T1/override-control");
    await observer.beginCall(call());
    await writeFile(join(project, "effect.txt"), "override undeclared effect\n");
    evidence.push(
      await expectFailure(
        "test-override-undeclared-disk-effect",
        "undeclared_disk_effect",
        () => observer.checkpointCall("control-call", "tool-result", "result"),
        "declared-post-result-effect",
      ),
    );
  }

  {
    const project = await freshProject(root, "post-result");
    const observer = new DiskStateObserver(project, "bash/T5/post-result-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("control-call", "initial-result", "result");
    await writeFile(join(project, "late.txt"), "post-result undeclared effect\n");
    evidence.push(
      await expectFailure(
        "post-result-undeclared-disk-effect",
        "undeclared_disk_effect",
        () => observer.checkpointCall("control-call", "task-terminal", "post_result"),
        "declared-post-result-effect",
      ),
    );
  }

  {
    const project = await freshProject(root, "declared-post-result");
    const observer = new DiskStateObserver(project, "bash/T5/declared-post-result");
    await observer.beginCall(
      call({
        name: "bash",
        outlives_result: true,
        disk_effects: [{ path: "late.txt", phase: "post_result" }],
      }),
    );
    await observer.checkpointCall("control-call", "initial-result", "result");
    await writeFile(join(project, "late.txt"), "declared post-result effect\n");
    await observer.checkpointCall("control-call", "task-terminal", "post_result");
    observer.markTerminal("control-call");
    await observer.finalize(true);
    evidence.push({
      control: "declared-post-result-effect",
      expected: "pass",
      observed: "pass",
      outcome: "passed",
    });
  }

  {
    const project = await freshProject(root, "cancellation");
    const observer = new DiskStateObserver(project, "bash/T4/cancellation-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("control-call", "initial-result", "result");
    await writeFile(join(project, "cancelled.txt"), "effect before confirmed stop\n");
    evidence.push(
      await expectFailure(
        "cancellation-undeclared-disk-effect",
        "undeclared_disk_effect",
        () => observer.checkpointCall("control-call", "cancel-confirmed", "cancellation"),
        "declared-post-result-effect",
      ),
    );
  }

  {
    const project = await freshProject(root, "surviving-writer");
    const observer = new DiskStateObserver(project, "bash/T5/surviving-writer-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("control-call", "initial-result", "result");
    evidence.push(
      await expectFailure(
        "surviving-writer-incomplete",
        "disk_effect_observation_incomplete",
        () => observer.finalize(false),
        "declared-post-result-effect",
      ),
    );
  }

  {
    const absent: PathState = { state: "absent" };
    const a: PathState = { state: "present", sha256: "a" };
    const b: PathState = { state: "present", sha256: "b" };
    const before = { created: absent, deleted: a, edited: a, source: a, destination: absent };
    const intermediate = {
      created: b,
      deleted: absent,
      edited: b,
      source: absent,
      destination: a,
    };
    assertThreeStateRestore(before, intermediate, before, [
      { path: "created", transition: "create", expected_intermediate_sha256: "b" },
      { path: "deleted", transition: "delete" },
      { path: "edited", transition: "edit" },
      { path: "source", transition: "move_source" },
      { path: "destination", transition: "move_destination", paired_path: "source" },
    ]);
    for (const control of [
      "three-state-create",
      "three-state-delete",
      "three-state-edit",
      "three-state-move",
    ]) {
      evidence.push({ control, expected: "pass", observed: "pass", outcome: "passed" });
    }
  }

  {
    const fixture = await provenanceFixture(root, "provenance-missing-source");
    await writeBuildInfo(fixture, { source: undefined });
    evidence.push(
      await expectFailure("provenance-missing-source", "executable_provenance", () =>
        verifyExecutableProvenance({
          executable: fixture.executable,
          repoRoot: fixture.repo,
          policyPath: fixture.policy,
          manifestPath: join(fixture.repo, "manifest.json"),
        }).then(() => undefined),
      ),
    );
  }

  {
    const fixture = await provenanceFixture(root, "provenance-disallowed-source");
    await writeBuildInfo(fixture, { source: "downloaded-release" });
    evidence.push(
      await expectFailure("provenance-disallowed-source", "executable_provenance", () =>
        verifyExecutableProvenance({
          executable: fixture.executable,
          repoRoot: fixture.repo,
          policyPath: fixture.policy,
          manifestPath: join(fixture.repo, "manifest.json"),
        }).then(() => undefined),
      ),
    );
  }

  {
    const fixture = await provenanceFixture(root, "provenance-substituted-revision");
    await writeBuildInfo(fixture, { git_sha: "0".repeat(40) });
    evidence.push(
      await expectFailure("provenance-substituted-revision", "executable_provenance", () =>
        verifyExecutableProvenance({
          executable: fixture.executable,
          repoRoot: fixture.repo,
          policyPath: fixture.policy,
          manifestPath: join(fixture.repo, "manifest.json"),
        }).then(() => undefined),
      ),
    );
  }

  // Spawn the stand-in a transport-dead window installs at `AFT_BINARY_PATH`
  // and watch what it does, because its behaviour is what the rows using that
  // window are able to assert. Those rows assert that bash REFUSES the
  // command, which only holds for a transport failure whose outcome cannot be
  // determined. A stand-in that died before the bridge handed it anything
  // would instead prove the command was never sent - the one case bash is
  // allowed to re-run it through the host shell - and the row would then
  // report whichever outcome the timing happened to produce.
  {
    const project = await freshProject(root, "dead-transport-stub");
    const stub = await makeTransportDeadStub(project, process.execPath);
    await pointTransportDeadStub(stub, true);

    let child: Subprocess<"pipe", "pipe", "ignore">;
    try {
      child = Bun.spawn([stub.executable], { stdin: "pipe", stdout: "pipe", stderr: "ignore" });
    } catch (error) {
      throw new Error(
        `the transport-dead stand-in could not be spawned, so a dead window would fail before the bridge sent it anything: ${error instanceof Error ? error.message : String(error)}`,
      );
    }

    await Bun.sleep(250);
    if (child.exitCode !== null) {
      throw new Error(
        `the transport-dead stand-in exited (${child.exitCode}) before any request was written, which proves nothing was sent and lets the break-glass path run the command`,
      );
    }

    child.stdin.write('{"id":"1","command":"configure"}\n');
    await child.stdin.flush();
    const exitCode = await child.exited;
    const answered = await new Response(child.stdout).text();
    if (exitCode === 0 || answered.length > 0) {
      throw new Error(
        `the transport-dead stand-in answered the request (exit ${exitCode}, ${answered.length} bytes), so the transport would report an outcome instead of leaving it undetermined`,
      );
    }
    evidence.push({
      control: "dead-transport-stub-accepts-then-dies",
      expected: "pass",
      observed: `accepted one request, then exited ${exitCode} without answering`,
      outcome: "passed",
    });
  }

  // The other half of the same claim: a row that declares a refusal has to
  // FAIL if the break-glass path runs the command after all. This is the
  // assertion a real run makes over the text the model received, driven here
  // with the banner the fallback puts on its own output.
  {
    const refusing: ScenarioDefinition = {
      schema_version: 1,
      id: "bash/T3/fallback_refusal_control",
      tool: "bash",
      trajectory: "T3",
      execution: "shared-server",
      prompt: "fallback refusal control",
      turns: [
        {
          label: "control-call",
          response: {
            kind: "tool_calls",
            calls: [call({ name: "bash", arguments: { command: "printf control" } })],
          },
        },
      ],
      metadata: {
        permission: { operation: "bash:host-fallback", reply: "once" },
        dead_transport: {
          outcome: "refusal",
          refusal_names: ["Binary (crashed|killed)", "outcome is UNKNOWN"],
          host_fallback_coverage: "n/a:control-row",
          reason: "control row: the refusal is what this control mutates against",
        },
      },
    };
    evidence.push(
      await expectFailure(
        "refusing-row-rejects-host-fallback-output",
        "no_effect_observed",
        async () => {
          assertBashExecutionPathIdentity(refusing, {
            resultText: `${HOST_FALLBACK_OUTPUT_MARKER} - module transport down]\ncontrol`,
            events: [],
          });
        },
        "dead-transport-stub-accepts-then-dies",
      ),
    );
  }

  return evidence;
}
