import { afterEach, describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  type HostCliContract,
  loadHostSchemaRejectionContract,
} from "./contracts.js";
import {
  assertThreeStateRestore,
  DiskStateObserver,
  snapshotPaths,
  type PathState,
} from "./disk-state.js";
import { HarnessError, type HarnessFailureCode } from "./errors.js";
import { runApiControl } from "./host.js";
import { readPermissionAskInventory } from "./inventory.js";
import { createScenarioIsolation } from "./isolation.js";
import { CompletionWakeLiveness, WatchPatternLiveness } from "./liveness.js";
import { materializeTurnPlaceholders, observeThenRespond } from "./mock-server.js";
import { projectText } from "./projection.js";
import { verifyExecutableProvenance } from "./provenance.js";
import { loadScenarios, materializeParityScenarios } from "./scenario-loader.js";
import { assertTurnLog } from "./turn-log.js";
import { resolveTransportDeadWindow, transportDeadAtTurn } from "./transport-window.js";
import type { ScenarioDefinition, ScriptedTurn, ToolCallPlan } from "./types.js";
import {
  applyMutatingTestOverride,
  deriveListSurfaces,
  deriveMutatingTools,
  deriveV2HarnessProjection,
  loadToolSchemas,
  validateHarnessInputs,
  validateInventory,
  validateMutatingDeclarations,
} from "./validation.js";
import { sha256File } from "./util.js";

const roots: string[] = [];

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

async function root(): Promise<string> {
  const path = await mkdtemp(join(tmpdir(), "opencode2-harness-test-"));
  roots.push(path);
  return path;
}

function call(overrides: Partial<ToolCallPlan> = {}): ToolCallPlan {
  return {
    id: "call-1",
    name: "write",
    arguments: { filePath: "changed.txt", content: "changed\n" },
    ...overrides,
  };
}

function scenario(toolCall: ToolCallPlan): ScenarioDefinition {
  return {
    schema_version: 1,
    id: "write/T1/control",
    tool: "write",
    trajectory: "T1",
    execution: "standalone",
    prompt: "control",
    turns: [
      {
        label: "turn-1",
        response: { kind: "tool_calls", calls: [toolCall] },
      },
    ],
  };
}

async function expectCode(
  action: () => Promise<unknown>,
  code: HarnessFailureCode,
): Promise<HarnessError> {
  try {
    await action();
  } catch (error) {
    expect(error).toBeInstanceOf(HarnessError);
    expect((error as HarnessError).code).toBe(code);
    return error as HarnessError;
  }
  throw new Error(`expected ${code}`);
}

describe("scenario isolation and liveness", () => {
  test("all host roots are private and the plugin source is a tarball", async () => {
    const parent = await root();
    const fixture = join(parent, "fixture");
    await mkdir(fixture);
    await writeFile(join(fixture, "file.txt"), "fixture\n");
    const tarball = join(parent, "plugin.tgz");
    await writeFile(tarball, "pack");
    const isolated = await createScenarioIsolation({
      parent: join(parent, "runs"),
      scenarioId: "read/T1/happy",
      fixture,
      pluginTarball: tarball,
      mockBaseUrl: "http://127.0.0.1:1234",
      providerConfig: {
        mock: {
          package: "observed-package",
          settings: { baseURL: "{{AIMOCK_BASE_URL}}/v1" },
          models: { "mock-model": { name: "Mock" } },
        },
      },
    });
    for (const key of [
      "HOME",
      "XDG_CONFIG_HOME",
      "XDG_DATA_HOME",
      "XDG_STATE_HOME",
      "XDG_CACHE_HOME",
      "XDG_RUNTIME_DIR",
    ]) {
      expect(isolated.env[key]?.startsWith(isolated.root)).toBe(true);
    }
    expect(isolated.env.OPENCODE_DISABLE_DEFAULT_PLUGINS).toBe("true");
    const hostConfig = JSON.parse(await readFile(isolated.host_config, "utf8"));
    expect(hostConfig.provider.mock.settings.baseURL).toBe("http://127.0.0.1:1234/v1");
  });

  test("turn-log liveness rejects a missing later turn", () => {
    expect(() => assertTurnLog(["tool", "result", "final"], ["tool", "result"])).toThrow(
      "turn_log_incomplete",
    );
  });

  test("shape projection refuses unparsed agent-visible text", () => {
    expect(() =>
      projectText("count: 2\nunparsed", [
        {
          kind: "field",
          field: "count",
          pattern: "^count: (?<value>\\d+)$",
          types: { value: "number" },
        },
      ]),
    ).toThrow("projection_unparsed:unparsed");
  });
});

describe("shared-server controls", () => {
  test("control path interpolation resolves permission, session, and task ids", async () => {
    const parent = await root();
    const executable = join(parent, "capture-api-arguments");
    const argumentsPath = join(parent, "arguments.txt");
    await writeFile(executable, '#!/bin/sh\nprintf "%s\\n" "$@" > "$ARGUMENTS_PATH"\n');
    await chmod(executable, 0o755);
    const handoff = { kind: "flag", name: "--server" } as const;
    const contract: HostCliContract = {
      schema_version: 1,
      host_version: "test",
      observed_run_id: "test",
      endpoint_handoff: { run: handoff, api: handoff },
      password_handoff: {
        run: { kind: "env", name: "OPENCODE_SERVER_PASSWORD" },
        api: { kind: "env", name: "OPENCODE_SERVER_PASSWORD" },
      },
      session_start: {},
      idle_retention: {},
      shared_server_smoke: { method: "GET", path: "/api/health" },
    };

    await runApiControl(
      {
        id: "interpolation-control",
        after_turn: "turn",
        method: "POST",
        path: "/api/session/{{session_id}}/permission/{{permission_id}}/task/{{task_id:source}}",
        purpose: "smoke",
      },
      {
        executable,
        cwd: parent,
        env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
        contract,
        endpoint: "http://127.0.0.1:4096",
        controlPathValues: {
          session_id: "session/one",
          permission_id: "permission two",
          "task_id:source": "bash-three",
        },
      },
    );

    expect((await readFile(argumentsPath, "utf8")).trim().split("\n")).toEqual([
      "api",
      "--server",
      "http://127.0.0.1:4096",
      "POST",
      "/api/session/session%2Fone/permission/permission%20two/task/bash-three",
    ]);
  });

  test("mock hooks observe generated ids before materializing the response", async () => {
    const turn: ScriptedTurn = {
      label: "arm-watch",
      response: {
        kind: "tool_calls",
        calls: [
          {
            id: "watch",
            name: "bash_watch",
            arguments: { taskId: "{{task_id:source}}" },
            non_mutating_evidence: { reason: "the unit fixture only materializes arguments" },
          },
        ],
      },
    };
    const order: string[] = [];
    const { response, exchange } = await observeThenRespond(
      turn,
      { messages: [{ role: "tool", content: "taskId: bash-generated" }] },
      0,
      {
        afterRequest: (exchange, observedTurn) => {
          order.push("observe");
          const taskId = JSON.stringify(exchange.request).match(/\bbash-[A-Za-z0-9_-]+\b/)?.[0];
          if (!observedTurn) throw new Error("scripted turn was not supplied to its request hook");
          materializeTurnPlaceholders(observedTurn, {
            "task_id:source": taskId ?? "",
          });
        },
        afterResponse: () => {
          order.push("respond");
        },
      },
    );

    expect(order).toEqual(["observe", "respond"]);
    expect(JSON.parse(String((response.toolCalls as Array<{ arguments: string }>)[0].arguments))).toEqual(
      { taskId: "bash-generated" },
    );
    expect(exchange.response).toBe(response);
  });

  test("fallback transport revives before the product restore turn", () => {
    const fallback: ScenarioDefinition = {
      schema_version: 1,
      id: "bash/T3/fallback_ask_allow",
      tool: "bash",
      trajectory: "T3",
      execution: "shared-server",
      prompt: "exercise one transient fallback window",
      turns: [
        {
          label: "checkpoint",
          response: {
            kind: "tool_calls",
            calls: [
              {
                id: "checkpoint",
                name: "aft_safety",
                arguments: { op: "checkpoint", name: "fallback" },
              },
            ],
          },
        },
        {
          label: "fallback-call",
          response: {
            kind: "tool_calls",
            calls: [{ id: "fallback", name: "bash", arguments: { command: "printf fallback" } }],
          },
        },
        {
          label: "restore",
          response: {
            kind: "tool_calls",
            calls: [
              {
                id: "restore",
                name: "aft_safety",
                arguments: { op: "restore", name: "fallback" },
              },
            ],
          },
        },
      ],
    };
    const window = resolveTransportDeadWindow(fallback);
    expect(window).toBeDefined();
    expect(
      fallback.turns.map((turn) => transportDeadAtTurn(fallback, window!, turn.label)),
    ).toEqual([false, true, false]);
  });
});

describe("source-of-truth derivation", () => {
  test("matrix drift guard rejects missing and removed tool rows by name", () => {
    const cells = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [trajectory, "n/a:test"]),
    ) as never;
    const powershell = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [trajectory, "n/a:platform"]),
    ) as never;
    const projectedControls = ["bash_kill", "bash_status", "bash_watch", "bash_write"];
    const requiredRows = [
      ...projectedControls.map((tool) => ({ tool, trajectories: cells })),
      { tool: "powershell", trajectories: powershell },
    ];
    const projection = ["read", ...projectedControls];
    const schemas = { read: {}, powershell: {} };
    expect(() =>
      validateInventory(
        { schema_version: 1, platform: "linux", rows: requiredRows },
        schemas,
        "linux",
        projection,
      ),
    ).toThrow("tool missing matrix row: read");
    expect(() =>
      validateInventory(
        {
          schema_version: 1,
          platform: "linux",
          rows: [
            { tool: "read", trajectories: cells },
            ...requiredRows,
            { tool: "removed", trajectories: cells },
          ],
        },
        schemas,
        "linux",
        projection,
      ),
    ).toThrow("matrix row names removed tool: removed");
  });

  test("V2 projection inventory matches schema through explicit exclusions in both directions", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests", "docker", "opencode2", "scenarios")),
    );
    const projection = deriveV2HarnessProjection(scenarios, "linux");
    const schemas = await loadToolSchemas(repo);
    const cells = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [
        trajectory,
        "n/a:test",
      ]),
    ) as never;
    const powershell = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [
        trajectory,
        "n/a:platform",
      ]),
    ) as never;
    const inventory = [...new Set([...projection, "powershell"])].sort();
    const matrix = {
      schema_version: 1 as const,
      platform: "linux",
      rows: inventory.map((tool) => ({
        tool,
        trajectories: tool === "powershell" ? powershell : cells,
      })),
    };

    expect(validateInventory(matrix, schemas, "linux", projection)).toEqual(inventory);
    const schemaWithProjectedControl = { ...schemas, bash_status: {} };
    expect(() => validateInventory(matrix, schemaWithProjectedControl, "linux", projection)).toThrow(
      "projection-only tools do not match the explicit exclusion table",
    );
    const { powershell: _powershell, ...schemaWithoutPlatformTool } = schemas;
    expect(() =>
      validateInventory(matrix, schemaWithoutPlatformTool, "linux", projection),
    ).toThrow("schema-only tools do not match the explicit exclusion table");
  });

  test("scenario validation tolerates matrix_absent until the full run", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests", "docker", "opencode2", "scenarios", "read")),
    );
    const optional = await validateHarnessInputs({
      repoRoot: repo,
      scenarios,
      pinnedHostVersion: "0.0.0-beta-test",
      platform: "linux",
      observationOnly: true,
    });
    expect(optional.matrix).toBeUndefined();
    expect(optional.inventory).toEqual(["read"]);

    await expectCode(
      () =>
        validateHarnessInputs({
          repoRoot: repo,
          scenarios,
          pinnedHostVersion: "0.0.0-beta-test",
          platform: "linux",
          observationOnly: true,
          fullRun: true,
        }),
      "matrix_absent",
    );
  });

  test("T7 is materialized from the same T1 scenario data", () => {
    const t1: ScenarioDefinition = {
      schema_version: 1,
      id: "read/T1/happy",
      tool: "read",
      trajectory: "T1",
      execution: "standalone",
      prompt: "read",
      turns: [{ label: "done", response: { kind: "text", content: "done" } }],
      comparison: { mode: "exact", expected: "text" },
      compare_call_id: "call",
    };
    const parity = materializeParityScenarios([t1]).find(
      (candidate) => candidate.id === "read/T7/happy",
    );
    expect(parity?.turns).toEqual(t1.turns);
    expect(parity?.comparison).toEqual(t1.comparison);
  });

  test("all eleven list surfaces and supported reasons come from the Rust registry", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const surfaces = await deriveListSurfaces(repo);
    expect(surfaces).toHaveLength(11);
    expect(
      surfaces.find((surface) => surface.id === "callgraph.trace_data.payload.hops")?.reasons,
    ).toEqual(["depth"]);
    expect(
      surfaces.find((surface) => surface.id === "outline.files.payload.files")?.reasons,
    ).toEqual(["budget", "walk"]);
  });

  test("mutating tools derive from product permission metadata in both directions", async () => {
    const repo = join(import.meta.dir, "../../../..");
    expect([...(await deriveMutatingTools(repo))].sort()).toEqual([
      "apply_patch",
      "ast_replace",
      "bash",
      "delete",
      "edit",
      "import",
      "move",
      "safety",
      "write",
    ]);
  });

  test("permission operations come from the exact exported inventory", async () => {
    const repo = join(import.meta.dir, "../../../..");
    expect(await readPermissionAskInventory(repo)).toEqual([
      "read",
      "edit",
      "write",
      "apply_patch",
      "aft_delete",
      "aft_move",
      "bash:withPermissionLoop",
      "bash:host-fallback",
    ]);
  });
});

describe("background liveness clocks", () => {
  test("watch setup, stdout antecedent, delivery, and duplicate window pass", () => {
    const clock = new WatchPatternLiveness({
      scenario: "bash/T5/watch_pattern_once",
      pattern: "READY",
      duplicateWindowMs: 100,
    });
    clock.accepted(0);
    clock.watchArmed(1);
    clock.taskStdout("READY", 2);
    clock.taskStdout("READY", 3);
    clock.delivery(4);
    expect(() => clock.assertComplete(104)).not.toThrow();
  });

  test("watch fixture with no first match fails its setup clock", () => {
    const clock = new WatchPatternLiveness({
      scenario: "bash/T5/watch_pattern_once",
      pattern: "READY",
      duplicateWindowMs: 100,
    });
    clock.accepted(0);
    clock.watchArmed(1);
    expect(() => clock.assertAt(30_000)).toThrow("setup_timeout:first_match");
  });

  test("a second completion cannot re-inject the first prompt", () => {
    const clock = new CompletionWakeLiveness({
      scenario: "bash/T5/completion_wake",
      rearmWindowMs: 100,
    });
    clock.taskStarted("first", 0);
    clock.promptInjected("first", "prompt-1", 1);
    clock.taskStarted("second", 2);
    expect(() => clock.promptInjected("second", "prompt-1", 3)).toThrow("duplicate_delivery");
  });
});

describe("tagged disk states and restore", () => {
  test("three-state create, delete, edit, and move transitions are non-vacuous", () => {
    const absent: PathState = { state: "absent" };
    const a: PathState = { state: "present", sha256: "a" };
    const b: PathState = { state: "present", sha256: "b" };
    const before = { create: absent, delete: a, edit: a, source: a, destination: absent };
    const intermediate = { create: b, delete: absent, edit: b, source: absent, destination: a };
    const restored = { ...before };
    expect(() =>
      assertThreeStateRestore(before, intermediate, restored, [
        { path: "create", transition: "create", expected_intermediate_sha256: "b" },
        { path: "delete", transition: "delete" },
        { path: "edit", transition: "edit" },
        { path: "source", transition: "move_source" },
        { path: "destination", transition: "move_destination", paired_path: "source" },
      ]),
    ).not.toThrow();
  });

  test("skip-mutation control fails at the intermediate checkpoint", () => {
    const present: PathState = { state: "present", sha256: "same" };
    expect(() =>
      assertThreeStateRestore({ file: present }, { file: present }, { file: present }, [
        { path: "file", transition: "edit" },
      ]),
    ).toThrow("no_effect_observed:file");
  });

  test("absent paths are tagged instead of hashed", async () => {
    const project = await root();
    expect(await snapshotPaths(project, ["missing.txt"])).toEqual({
      "missing.txt": { state: "absent" },
    });
  });
});

describe("whole-root asynchronous observation controls", () => {
  test("ordinary-undeclared-disk-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "write/T1/ordinary-control");
    await observer.beginCall(call());
    await writeFile(join(project, "changed.txt"), "undeclared\n");
    const error = await expectCode(
      () => observer.checkpointCall("call-1", "tool-result", "result"),
      "undeclared_disk_effect",
    );
    expect(error.details.path).toBe("changed.txt");
  });

  test("test-override-undeclared-disk-effect", async () => {
    const override = applyMutatingTestOverride(
      new Set(["write"]),
      {
        AFT_OPENCODE2_HARNESS_SELF_TEST: "1",
        AFT_OPENCODE2_TEST_REMOVE_MUTATING_TOOL: "write",
        AFT_OPENCODE2_TEST_NON_MUTATING_EVIDENCE: "fabricated control evidence",
      },
      true,
    );
    validateMutatingDeclarations([scenario(call())], override.tools, override.fabricatedEvidence);

    const project = await root();
    const observer = new DiskStateObserver(project, "write/T1/override-control");
    await observer.beginCall(call());
    await writeFile(join(project, "changed.txt"), "still observed\n");
    await expectCode(
      () => observer.checkpointCall("call-1", "tool-result", "result"),
      "undeclared_disk_effect",
    );
  });

  test("post-result-undeclared-disk-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T5/post-result-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("call-1", "initial-result", "result");
    await writeFile(join(project, "late.txt"), "after result\n");
    const error = await expectCode(
      () => observer.checkpointCall("call-1", "task-terminal", "post_result"),
      "undeclared_disk_effect",
    );
    expect(error.details.phase).toBe("post_result");
  });

  test("declared-post-result-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T5/declared-post-result");
    await observer.beginCall(
      call({
        name: "bash",
        outlives_result: true,
        disk_effects: [{ path: "late.txt", phase: "post_result" }],
      }),
    );
    await observer.checkpointCall("call-1", "initial-result", "result");
    await writeFile(join(project, "late.txt"), "after result\n");
    await observer.checkpointCall("call-1", "task-terminal", "post_result");
    observer.markTerminal("call-1");
    await observer.finalize(true);
  });

  test("cancellation-undeclared-disk-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T4/cancellation-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("call-1", "initial-result", "result");
    await writeFile(join(project, "cancelled.txt"), "before stop\n");
    const error = await expectCode(
      () => observer.checkpointCall("call-1", "cancel-confirmed", "cancellation"),
      "undeclared_disk_effect",
    );
    expect(error.details.phase).toBe("cancellation");
  });

  test("surviving-writer-incomplete", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T5/surviving-writer");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("call-1", "initial-result", "result");
    await expectCode(() => observer.finalize(false), "disk_effect_observation_incomplete");
  });
});

describe("producer-backed executable provenance", () => {
  async function fixture(): Promise<{
    repo: string;
    executable: string;
    policy: string;
    manifest: string;
    sha: string;
  }> {
    const repo = await root();
    Bun.spawnSync(["git", "init", "-q"], { cwd: repo });
    await writeFile(join(repo, "tracked"), "content\n");
    Bun.spawnSync(["git", "add", "tracked"], { cwd: repo });
    Bun.spawnSync(
      [
        "git",
        "-c",
        "user.name=Harness",
        "-c",
        "user.email=harness@example.invalid",
        "commit",
        "-qm",
        "fixture",
      ],
      { cwd: repo },
    );
    const sha = Bun.spawnSync(["git", "rev-parse", "HEAD"], { cwd: repo }).stdout.toString().trim();
    const executable = join(repo, "aft");
    await writeFile(executable, `#!/bin/sh\necho "aft test (${sha})"\n`);
    await chmod(executable, 0o755);
    await writeFile(
      join(repo, "build-info.json"),
      `${JSON.stringify({
        git_sha: sha,
        build_profile: "release",
        sha256: await sha256File(executable),
        source: "checkout-build",
      })}\n`,
    );
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
    return { repo, executable, policy, manifest: join(repo, "run", "aft_binary.json"), sha };
  }

  test("verified producer sidecar is copied into the run manifest", async () => {
    const item = await fixture();
    const manifest = await verifyExecutableProvenance({
      executable: item.executable,
      repoRoot: item.repo,
      policyPath: item.policy,
      manifestPath: item.manifest,
    });
    expect(manifest.git_sha).toBe(item.sha);
    expect(manifest.source).toBe("checkout-build");
    expect(manifest.verdict).toBe("verified");
  });

  test("missing producer source is unsuppressibly rejected", async () => {
    const item = await fixture();
    const sidecar = JSON.parse(await Bun.file(join(item.repo, "build-info.json")).text());
    delete sidecar.source;
    await writeFile(join(item.repo, "build-info.json"), JSON.stringify(sidecar));
    const error = await expectCode(
      () =>
        verifyExecutableProvenance({
          executable: item.executable,
          repoRoot: item.repo,
          policyPath: item.policy,
          manifestPath: item.manifest,
        }),
      "executable_provenance",
    );
    expect(error.unsuppressible).toBe(true);
  });
});

test("missing host schema rejection contract fails validation instead of earning coverage", async () => {
  const contractRoot = await root();
  await expectCode(
    () => loadHostSchemaRejectionContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
});
