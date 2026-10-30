import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { HostEvent } from "../../harness/event-stream.js";
import {
  assertBashAftExecutionIdentity,
  assertBashDeadTransportRefusal,
  assertBashExecutionPathIdentity,
  assertConfigDenyHidesTool,
  HOST_FALLBACK_ASK_MARKER,
  HOST_FALLBACK_OUTPUT_MARKER,
} from "../../harness/permission-plan.js";
import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import type { RecordedMockExchange, ScenarioDefinition } from "../../harness/types.js";
import extension, { judgeMessageDetachEvent } from "./bash.extension.js";
import {
  judgeLinkedWorktreeResults,
  MAIN_CHECKOUT_DIRECTORY,
  PWD_CALL_ID,
  PWD_MARKER,
  prepareLinkedWorktree,
  READ_CALL_ID,
  WORKTREE_ONLY_CONTENT,
  WORKTREE_ONLY_FILE,
} from "./linked-worktree.js";

const loaded = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
const scenarios = new Map(loaded.map((scenario) => [scenario.id, scenario]));

function scenario(id: string): ScenarioDefinition {
  const found = scenarios.get(id);
  if (!found) throw new Error(`scenario not registered: ${id}`);
  return found;
}

/** A model request as the host builds it, offering the named tools. */
function exchange(label: string, offeredTools: readonly string[]): RecordedMockExchange {
  return {
    index: 0,
    label,
    request: {
      tools: offeredTools.map((name) => ({ type: "function", function: { name } })),
    },
    response: {},
    observed_at: "1970-01-01T00:00:00.000Z",
  };
}

function askedFor(text: string): HostEvent {
  return { type: "permission.asked", data: { action: "bash", resources: [text] } };
}

/**
 * What the plugin hands the model when the standalone transport dies, as one
 * bash/T3 fallback row recorded it.
 *
 * The bridge writes the request to the binary it spawned, so a binary that
 * then crashes or is killed leaves the outcome undetermined; the message
 * carries that disposition, and it is the whole reason bash refuses rather
 * than re-running the command in the host. The log path is the running
 * scenario's own and is elided here.
 */
const refusal =
  "[aft-bridge] Binary crashed (restarts: 0) (see /tmp/aft-plugin.log) " +
  "The standalone AFT transport failed after this call may have been sent, so its outcome is " +
  "UNKNOWN: it may or may not have executed. Verify actual state before re-running, and never " +
  "blind-retry a mutation.";

describe("bash OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    expect(loaded.length).toBe(24);
    await extension.validate?.({
      repo_root: resolve(import.meta.dir, "../../../../../.."),
      platform: "linux",
      scenarios: loaded,
      matrix: {},
      pinned_host_version: "2.0.3",
    });
  });

  test("changed-files T6 pairs enable their user-only mode and declare real writes", () => {
    for (const [name, mode, surface, incompleteEffects] of [
      ["bash-changed-files", "list", "bash.changed_files.bash.changed_files", 41],
      ["bash-diff-lines", "diff", "bash.diff_lines.bash.diff_lines", 1],
    ] as const) {
      for (const kind of ["complete", "incomplete"] as const) {
        const row = scenario(`bash/T6/${name}/${kind}`);
        expect(row.user_config).toEqual({ bash: { changed_files: mode } });
        expect(row.project_config).toBeUndefined();
        expect(row.metadata?.t6).toMatchObject({ surface_id: surface, owner: "bash", fixture: kind });
        const call = row.turns.flatMap((turn) => turn.response.kind === "tool_calls" ? turn.response.calls : [])
          .find((call) => call.id === row.compare_call_id);
        expect(call?.arguments).toMatchObject({ wait: true, compressed: false });
        expect(call?.disk_effects).toHaveLength(kind === "complete" ? 1 : incompleteEffects);
        expect(call?.non_mutating_evidence).toBeUndefined();
        if (kind === "incomplete") {
          expect(row.metadata?.t6).toMatchObject({ triggered_reason: "cap" });
          expect(call?.arguments.command).toContain(mode === "list" ? "-lt 41" : "-lt 250");
        }
      }
    }
  });
});

describe("bash abort ordering", () => {
  test("waits for the Rust task row instead of guessing with a fixed delay", () => {
    const abort = scenario("bash/T4/abort");
    const control = abort.controls?.find((candidate) => candidate.purpose === "abort");

    expect(control?.delay_ms).toBeUndefined();
    expect(control?.wait_for_task).toEqual({ status: "running", timeout_ms: 10_000 });
  });
});

describe("a configured denial hides the tool it names", () => {
  const denied = scenario("bash/T3/loop_config_deny");

  // Permission rows run on the OpenCode 2 host, where AFT's bash is offered
  // as `shell`; the host's own shell plugin is removed by the isolation config.
  test("accepts a tool the host offered before the rule and dropped after it", () => {
    expect(() =>
      assertConfigDenyHidesTool(
        denied,
        [exchange("call-tool", ["shell", "read"]), exchange("finish", ["read"])],
        "v2",
      ),
    ).not.toThrow();
  });

  test("rejects a tool the host never offered, which never registered rather than being hidden", () => {
    expect(() =>
      assertConfigDenyHidesTool(
        denied,
        [exchange("call-tool", ["bash", "read"]), exchange("finish", ["read"])],
        "v2",
      ),
    ).toThrow("the host never offered shell");
  });

  test("rejects a tool the host kept offering under the rule", () => {
    expect(() =>
      assertConfigDenyHidesTool(
        denied,
        [exchange("call-tool", ["shell", "read"]), exchange("finish", ["shell", "read"])],
        "v2",
      ),
    ).toThrow("kept offering shell");
  });

  test("an OpenCode 1 host names the tool bash", () => {
    expect(() =>
      assertConfigDenyHidesTool(
        denied,
        [exchange("call-tool", ["bash", "read"]), exchange("finish", ["read"])],
        "v1",
      ),
    ).not.toThrow();
  });

  test("rejects a run that never reached the model", () => {
    expect(() => assertConfigDenyHidesTool(denied, [])).toThrow("offered no tools");
  });

  test("says nothing about rows whose permission is answered rather than configured", () => {
    expect(() =>
      assertConfigDenyHidesTool(scenario("bash/T3/loop_ask_deny"), [
        exchange("call-tool", ["read"]),
      ]),
    ).not.toThrow();
  });
});

describe("the break-glass ask belongs to the path that can prove the command was never sent", () => {
  test("a loop row that fell back to host execution is the wrong path", () => {
    expect(() =>
      assertBashExecutionPathIdentity(scenario("bash/T3/loop_ask_allow"), {
        resultText: `${HOST_FALLBACK_OUTPUT_MARKER} - module transport down]\nok`,
        events: [],
      }),
    ).toThrow("fell back to host execution");
  });

  test("a rule-denied row that raised the break-glass ask found a way around the denial", () => {
    expect(() =>
      assertBashExecutionPathIdentity(scenario("bash/T3/fallback_config_deny"), {
        resultText: "",
        events: [askedFor(`${HOST_FALLBACK_ASK_MARKER} (transport down) - host fallback`)],
      }),
    ).toThrow("raised the host-fallback ask");
  });

  test("a rule-denied fallback row needs no ask, because the host never dispatches the tool", () => {
    expect(() =>
      assertBashExecutionPathIdentity(scenario("bash/T3/fallback_config_deny"), {
        resultText: "Permission denied: this session's permission rules deny bash",
        events: [],
      }),
    ).not.toThrow();
  });

  test("a row declaring a refusal accepts the absence of the mark, under either answer", () => {
    for (const id of ["bash/T3/fallback_ask_allow", "bash/T3/fallback_ask_deny"]) {
      expect(() =>
        assertBashExecutionPathIdentity(scenario(id), { resultText: refusal, events: [] }),
      ).not.toThrow();
    }
  });

  test("a row declaring a refusal refuses the mark: the command may already have been sent", () => {
    for (const id of ["bash/T3/fallback_ask_allow", "bash/T3/fallback_ask_deny"]) {
      expect(() =>
        assertBashExecutionPathIdentity(scenario(id), {
          resultText: `${HOST_FALLBACK_OUTPUT_MARKER} - transport down]\nfallback permission fixture`,
          events: [],
        }),
      ).toThrow("which can run a command that was already sent");
    }
  });

  test("a row declaring the break-glass path still has to reach its ask", () => {
    const declared = scenario("bash/T3/fallback_ask_allow");
    const exercised: ScenarioDefinition = {
      ...declared,
      metadata: {
        ...declared.metadata,
        dead_transport: { outcome: "host_fallback", reason: "an admitted pre-dispatch failure" },
      },
    };

    expect(() =>
      assertBashExecutionPathIdentity(exercised, { resultText: refusal, events: [] }),
    ).toThrow("raised no host-fallback ask");
    expect(() =>
      assertBashExecutionPathIdentity(exercised, {
        resultText: "",
        events: [askedFor(`${HOST_FALLBACK_ASK_MARKER} (module down) - host fallback`)],
      }),
    ).not.toThrow();
  });

  test("a loop row with neither mark ran through AFT, as its configuration declares", () => {
    expect(() =>
      assertBashExecutionPathIdentity(scenario("bash/T3/loop_ask_allow"), {
        resultText: "loop permission fixture",
        events: [askedFor("printf 'loop permission fixture' > bash-loop.txt")],
      }),
    ).not.toThrow();
  });

  test("a fallback row must say what its dead transport produces", () => {
    const declared = scenario("bash/T3/fallback_ask_deny");
    const undeclared: ScenarioDefinition = {
      ...declared,
      metadata: { permission: declared.metadata?.permission },
    };

    expect(() =>
      assertBashExecutionPathIdentity(undeclared, { resultText: refusal, events: [] }),
    ).toThrow("must declare what its dead transport produces");
  });

  test("a refusing row must record why break-glass execution is not covered instead", () => {
    const declared = scenario("bash/T3/fallback_ask_deny");
    const unrecorded: ScenarioDefinition = {
      ...declared,
      metadata: {
        ...declared.metadata,
        dead_transport: {
          outcome: "refusal",
          refusal_names: ["outcome is UNKNOWN"],
          reason: "the transport cannot say the command was never sent",
        },
      },
    };

    expect(() =>
      assertBashExecutionPathIdentity(unrecorded, { resultText: refusal, events: [] }),
    ).toThrow("n/a:<reason>");
  });
});

describe("a refusing row refused, in the model's own view of the call", () => {
  test("the refusal names the transport that failed and the outcome nobody can determine", () => {
    for (const id of ["bash/T3/fallback_ask_allow", "bash/T3/fallback_ask_deny"]) {
      expect(() =>
        assertBashDeadTransportRefusal(scenario(id), { resultText: refusal, events: [] }),
      ).not.toThrow();
    }
  });

  test("a bare transport failure leaves the agent with nothing to act on", () => {
    expect(() =>
      assertBashDeadTransportRefusal(scenario("bash/T3/fallback_ask_allow"), {
        resultText: "[aft-bridge] Binary crashed (restarts: 0) (see /tmp/aft-plugin.log)",
        events: [],
      }),
    ).toThrow("does not name outcome is UNKNOWN");
  });

  test("a refusal that names no observed failure could be any refusal at all", () => {
    expect(() =>
      assertBashDeadTransportRefusal(scenario("bash/T3/fallback_ask_allow"), {
        resultText: "bash failed",
        events: [],
      }),
    ).toThrow("does not name Binary (crashed|killed)");
  });

  test("an ask raised by the refused call means it reached an ask site", () => {
    expect(() =>
      assertBashDeadTransportRefusal(scenario("bash/T3/fallback_ask_deny"), {
        resultText: refusal,
        events: [
          {
            type: "permission.asked",
            data: {
              action: "bash",
              resources: ["printf 'fallback permission fixture' > bash-fallback.txt"],
              source: { id: "bash-fallback-ask_deny" },
            },
          },
        ],
      }),
    ).toThrow("still raised a permission request");
  });

  test("says nothing about the rows that declare another outcome", () => {
    for (const id of ["bash/T3/fallback_config_deny", "bash/T3/loop_ask_allow"]) {
      expect(() =>
        assertBashDeadTransportRefusal(scenario(id), { resultText: "", events: [] }),
      ).not.toThrow();
    }
  });
});

describe("AFT task rows say whether AFT ran the command", () => {
  test("an allowed loop row without a task row means AFT ran nothing", () => {
    expect(() => assertBashAftExecutionIdentity(scenario("bash/T3/loop_ask_allow"), [])).toThrow(
      "left no task row",
    );
  });

  test("an allowed loop row with a task row ran through AFT", () => {
    expect(() =>
      assertBashAftExecutionIdentity(scenario("bash/T3/loop_ask_allow"), ["bash-1a2b3c4d"]),
    ).not.toThrow();
  });

  test("a denied row with a task row ran a command it was refused", () => {
    expect(() =>
      assertBashAftExecutionIdentity(scenario("bash/T3/loop_config_deny"), ["bash-1a2b3c4d"]),
    ).toThrow("still reached AFT");
  });

  test("an allowed fallback row with a task row kept a live transport", () => {
    expect(() =>
      assertBashAftExecutionIdentity(scenario("bash/T3/fallback_ask_allow"), ["bash-1a2b3c4d"]),
    ).toThrow("transport-dead window did not take");
  });

  test("denied rows without task rows ran nothing", () => {
    for (const id of ["bash/T3/loop_ask_deny", "bash/T3/fallback_config_deny"]) {
      expect(() => assertBashAftExecutionIdentity(scenario(id), [])).not.toThrow();
    }
  });
});

describe("a message sent mid-wait must end that wait promptly", () => {
  const rows = ["bash/T5/message_detaches_wait", "bash/T5/message_aborts_sync_watch"];

  function events(row: ScenarioDefinition) {
    const plan = row.metadata?.message_detach as { control_id: string; call_id: string };
    const control = row.controls?.find((candidate) => candidate.id === plan.control_id);
    const call = row.turns
      .flatMap((turn) => (turn.response.kind === "tool_calls" ? turn.response.calls : []))
      .find((candidate) => candidate.id === plan.call_id);
    if (!control || !call) throw new Error(`${row.id}: declaration names nothing`);
    return {
      sent: (at: number) => ({ kind: "control_started" as const, control, at }),
      result: (at: number) => ({
        kind: "tool_result_observed" as const,
        call,
        before_turn: "after",
        at,
      }),
    };
  }

  test("each row sends its message through the prompt route once the task runs", () => {
    for (const id of rows) {
      const row = scenario(id);
      const control = row.controls?.[0];
      expect(control?.purpose).toBe("message");
      expect(control?.path).toBe("/api/session/{{session_id}}/prompt");
      expect(control?.wait_for_task).toEqual({ status: "running", timeout_ms: 10_000 });
    }
  });

  test("a result soon after the message passes", () => {
    for (const id of rows) {
      const row = scenario(id);
      const { sent, result } = events(row);
      const sentAt = new Map<string, number>();
      judgeMessageDetachEvent(row, sent(1_000), sentAt, "run");
      expect(() => judgeMessageDetachEvent(row, result(2_500), sentAt, "run")).not.toThrow();
    }
  });

  test("a result that only arrives when the command finishes fails", () => {
    for (const id of rows) {
      const row = scenario(id);
      const { sent, result } = events(row);
      const sentAt = new Map<string, number>();
      judgeMessageDetachEvent(row, sent(1_000), sentAt, "run");
      expect(() => judgeMessageDetachEvent(row, result(39_000), sentAt, "run")).toThrow(
        "over the 10000ms budget",
      );
    }
  });

  test("a result that came back before any message was sent fails", () => {
    for (const id of rows) {
      const row = scenario(id);
      const { result } = events(row);
      expect(() => judgeMessageDetachEvent(row, result(500), new Map(), "run")).toThrow(
        "before the message was sent",
      );
    }
  });

  test("rows without the declaration are not judged", () => {
    const row = scenario("bash/T4/abort");
    const call = row.turns[0]?.response.kind === "tool_calls" ? row.turns[0].response.calls[0] : undefined;
    if (!call) throw new Error("bash/T4/abort has no call");
    expect(() =>
      judgeMessageDetachEvent(
        row,
        { kind: "tool_result_observed", call, before_turn: "finish", at: 99_000 },
        new Map(),
        "run",
      ),
    ).not.toThrow();
  });
});

describe("linked worktree row (issue #387)", () => {
  async function git(args: string[], cwd: string): Promise<string> {
    const child = Bun.spawnSync(["git", ...args], { cwd });
    if (child.exitCode !== 0) throw new Error(`git ${args.join(" ")}: ${child.stderr.toString()}`);
    return child.stdout.toString().trim();
  }

  /**
   * A project in the state the harness leaves it before any extension runs: a
   * git repository with one commit, plus the AFT config written after it.
   */
  async function harnessProject(): Promise<{ root: string; project: string }> {
    const root = await realpath(await mkdtemp(join(tmpdir(), "bash-linked-worktree-")));
    const project = join(root, "project");
    await mkdir(project);
    await writeFile(join(project, "sample.txt"), "alpha\n");
    await git(["init", "-q"], project);
    await git(["add", "-A"], project);
    await git(
      ["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-qm", "baseline"],
      project,
    );
    // Written after the baseline commit, as the harness writes the AFT config.
    await mkdir(join(project, ".cortexkit"));
    await writeFile(join(project, ".cortexkit", "aft.jsonc"), "{}\n");
    return { root, project };
  }

  function toolResults(results: Record<string, string>): RecordedMockExchange[] {
    return [
      {
        index: 2,
        label: "finish",
        request: {
          messages: Object.entries(results).map(([id, content]) => ({
            role: "tool",
            tool_call_id: id,
            content,
          })),
        },
        response: {},
        observed_at: "1970-01-01T00:00:00.000Z",
      },
    ];
  }

  test("the setup makes the project a linked worktree of a main checkout beside it", async () => {
    const { root, project } = await harnessProject();
    try {
      const mainCheckout = await prepareLinkedWorktree(project);
      expect(mainCheckout).toBe(join(root, MAIN_CHECKOUT_DIRECTORY));
      const list = await git(["worktree", "list", "--porcelain"], project);
      const worktrees = list
        .split("\n")
        .filter((line) => line.startsWith("worktree "))
        .map((line) => line.slice("worktree ".length));
      // The first entry is the main worktree, which OpenCode 2 reports as the
      // Location's project.canonical.
      expect(worktrees).toEqual([mainCheckout, project]);
      expect(await readFile(join(project, WORKTREE_ONLY_FILE), "utf8")).toBe(
        `${WORKTREE_ONLY_CONTENT}\n`,
      );
      expect(existsSync(join(mainCheckout, WORKTREE_ONLY_FILE))).toBe(false);
      expect(await readFile(join(mainCheckout, "sample.txt"), "utf8")).toBe("alpha\n");
      expect(await readFile(join(project, "sample.txt"), "utf8")).toBe("alpha\n");
      expect(await readFile(join(project, ".cortexkit", "aft.jsonc"), "utf8")).toBe("{}\n");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("the judge accepts the worktree and rejects the main checkout or a failed read", async () => {
    const { root, project } = await harnessProject();
    try {
      const mainCheckout = await prepareLinkedWorktree(project);
      const readOk = `1: ${WORKTREE_ONLY_CONTENT}\n`;
      await judgeLinkedWorktreeResults(
        project,
        toolResults({ [READ_CALL_ID]: readOk, [PWD_CALL_ID]: `${PWD_MARKER}${project}\n` }),
      );
      await expect(
        judgeLinkedWorktreeResults(
          project,
          toolResults({ [READ_CALL_ID]: readOk, [PWD_CALL_ID]: `${PWD_MARKER}${mainCheckout}\n` }),
        ),
      ).rejects.toThrow("(the main checkout)");
      await expect(
        judgeLinkedWorktreeResults(
          project,
          toolResults({
            [READ_CALL_ID]: `file not found: ${join(mainCheckout, WORKTREE_ONLY_FILE)}`,
            [PWD_CALL_ID]: `${PWD_MARKER}${project}\n`,
          }),
        ),
      ).rejects.toThrow("did not return the worktree file's content");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("the registered comparison projects the last segment of the printed directory", () => {
    const row = scenario("bash/T1/linked_worktree");
    const comparison = row.comparison;
    if (comparison?.mode !== "shape") throw new Error("expected a shape comparison");
    const rule = comparison.rules[0];
    if (rule.kind !== "field") throw new Error("expected a field rule");
    const leaf = (text: string) => new RegExp(rule.pattern).exec(text)?.groups?.value;
    expect(leaf(`${PWD_MARKER}/runs/x/project\n`)).toBe("project");
    expect(leaf(`${PWD_MARKER}/runs/x/${MAIN_CHECKOUT_DIRECTORY}`)).toBe(MAIN_CHECKOUT_DIRECTORY);
    expect(comparison.expected).toEqual({ cwd_leaf: "project" });
    expect(scenario("bash/T7/linked_worktree").metadata?.linked_worktree).toBe(true);
  });
});
