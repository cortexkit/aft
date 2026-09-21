import { afterEach, describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from 'node:url';

import { mapWithConcurrency, parseE2EConcurrency } from "./concurrency.js";
import {
  type HostCliContract,
  loadHostCliContract,
  loadHostProviderConfigContract,
  loadHostSchemaRejectionContract,
  loadV1HostProviderConfigContract,
} from "./contracts.js";
import {
  assertThreeStateRestore,
  DiskStateObserver,
  snapshotPaths,
  type PathState,
} from "./disk-state.js";
import { HarnessError, type HarnessFailureCode } from "./errors.js";
import type { HostEvent } from "./event-stream.js";
import { runApiControl, startScenarioClient, startSharedServer } from "./host.js";
import { readPermissionAskInventory } from "./inventory.js";
import { createScenarioIsolation } from "./isolation.js";
import {
  assertT5HostWakeTranscript,
  CompletionWakeLiveness,
  WatchPatternLiveness,
} from "./liveness.js";
import {
  hostToolArguments,
  hostToolName,
  isTitleGenerationRequest,
  materializeTurnPlaceholders,
  observeThenRespond,
  toolResultForCall,
} from "./mock-server.js";
import { assertT6Trailer, projectText, TRUNCATION_TRAILER_PATTERN } from "./projection.js";
import {
  assertPermissionPromptObserved,
  controlPlans,
  permissionAction,
  sessionPermissionRules,
} from "./permission-plan.js";
import { ProcessObserver } from "./process-observer.js";
import { parentDisposition, reportTable } from "./report.js";
import { verifyExecutableProvenance } from "./provenance.js";
import {
  addCallgraphWarmup,
  loadScenarios,
  materializeParityScenarios,
} from "./scenario-loader.js";
import { assertTurnLog } from "./turn-log.js";
import { resolveTransportDeadWindow, transportDeadAtTurn } from "./transport-window.js";
import type { ScenarioDefinition, ScenarioResult, ScriptedTurn, ToolCallPlan } from "./types.js";
import {
  applyMutatingTestOverride,
  deriveListSurfaces,
  deriveMutatingTools,
  deriveV2HarnessProjection,
  loadToolSchemas,
  validateHarnessInputs,
  validateParityAllowlist,
  validateT6,
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

describe("bounded scenario concurrency", () => {
  test("defaults to four and rejects invalid worker counts", () => {
    expect(parseE2EConcurrency(undefined)).toBe(4);
    expect(parseE2EConcurrency("1")).toBe(1);
    for (const invalid of ["0", "-1", "1.5", "not-a-number"]) {
      expect(() => parseE2EConcurrency(invalid)).toThrow("must be a positive integer");
    }
  });

  test("caps active work and preserves input order instead of completion order", async () => {
    const gates = Array.from({ length: 4 }, () => {
      let release = () => {};
      const promise = new Promise<void>((resolve) => {
        release = resolve;
      });
      return { promise, release };
    });
    const started: number[] = [];
    const completed: number[] = [];
    let active = 0;
    let maximumActive = 0;
    const running = mapWithConcurrency(["zero", "one", "two", "three"], 2, async (value, index) => {
      started.push(index);
      active += 1;
      maximumActive = Math.max(maximumActive, active);
      await gates[index].promise;
      active -= 1;
      completed.push(index);
      return value;
    });

    while (started.length < 2) await Bun.sleep(1);
    expect(started).toEqual([0, 1]);
    gates[1].release();
    while (started.length < 3) await Bun.sleep(1);
    gates[2].release();
    while (started.length < 4) await Bun.sleep(1);
    gates[3].release();
    gates[0].release();

    expect(await running).toEqual(["zero", "one", "two", "three"]);
    expect(completed).toEqual([1, 2, 3, 0]);
    expect(maximumActive).toBe(2);
  });
});

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
    const pluginDirectory = join(parent, "installed-plugin");
    await mkdir(join(pluginDirectory, "dist", "entry"), { recursive: true });
    await writeFile(join(pluginDirectory, "dist", "index.js"), "export default {};\n");
    await writeFile(join(pluginDirectory, "dist", "entry", "server.js"), "export default {};\n");
    const isolated = await createScenarioIsolation({
      parent: join(parent, "runs"),
      scenarioId: "read/T1/happy",
      fixture,
      pluginTarball: tarball,
      pluginDirectory,
      pluginVersion: "1.2.3-test",
      hostGeneration: "v2",
      binaryPath: "/native/aft",
      mockBaseUrl: "http://127.0.0.1:1234",
      providerConfig: {
        mock: {
          package: "observed-package",
          settings: { baseURL: "{{AIMOCK_BASE_URL}}/v1" },
          models: { "mock-model": { name: "Mock" } },
        },
      },
      providerConfigKey: "providers",
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
    expect(isolated.env.OPENCODE_DB).toBe("opencode2.db");
    expect(isolated.env.AFT_BINARY_PATH).toBe("/native/aft");
    const aftConfig = JSON.parse(
      await readFile(join(isolated.project, ".cortexkit", "aft.jsonc"), "utf8"),
    );
    expect(aftConfig).toMatchObject({
      tool_surface: "all",
      semantic_search: true,
      search_index: true,
    });
    const hostConfig = JSON.parse(await readFile(isolated.host_config, "utf8"));
    expect(hostConfig.plugin[0]).toEndWith("/xdg-config/aft-opencode-wrapper");
    expect(hostConfig.providers.mock.settings.baseURL).toBe("http://127.0.0.1:1234/v1");
    expect(hostConfig.provider).toBeUndefined();
    const serverWrapper = await readFile(
      join(isolated.config, "aft-opencode-wrapper", "index.mjs"),
      "utf8",
    );
    expect(serverWrapper).toContain(pathToFileURL(tarball).href);
    expect(serverWrapper).toContain("dist/entry/server.js");
    expect(serverWrapper).toContain("PLUGIN_VERSION=1.2.3-test");

    const legacy = await createScenarioIsolation({
      parent: join(parent, "legacy-runs"),
      scenarioId: "read/T7/legacy",
      fixture,
      pluginTarball: tarball,
      pluginDirectory,
      pluginVersion: "1.2.3-test",
      hostGeneration: "v1",
      mockBaseUrl: "http://127.0.0.1:1234",
      providerConfig: {
        mock: {
          api: "openai",
          options: { baseURL: "{{AIMOCK_BASE_URL}}/v1" },
          models: { "mock-model": { name: "Mock" } },
        },
      },
      providerConfigKey: "provider",
    });
    const legacyConfig = JSON.parse(await readFile(legacy.host_config, "utf8"));
    // The V1 host reads `provider`; it logs `providers` as an unsupported key
    // and then has no provider to resolve the run's model against.
    expect(legacyConfig.provider.mock.options.baseURL).toBe("http://127.0.0.1:1234/v1");
    expect(legacyConfig.providers).toBeUndefined();
    const legacyWrapper = await readFile(
      join(legacy.config, "aft-opencode-wrapper", "index.mjs"),
      "utf8",
    );
    expect(legacyWrapper).toContain("dist/index.js");
    expect(legacyWrapper).not.toContain("dist/entry/server.js");
    // V1 takes a plugin function, not the V2 object carrying `effect`.
    expect(legacyWrapper).toContain("export default (context) => {");
    expect(legacyWrapper).not.toContain("plugin.effect");
    expect(serverWrapper).toContain("plugin.effect");
  });

  test("the scenario client uses the provider contract model", async () => {
    const parent = await root();
    const executable = join(parent, "capture-run-arguments");
    const argumentsPath = join(parent, "arguments.txt");
    await writeFile(executable, '#!/bin/sh\nprintf "%s\\n" "$@" > "$ARGUMENTS_PATH"\n');
    await chmod(executable, 0o755);

    const client = startScenarioClient({
      executable,
      scenario: scenario(call()),
      cwd: parent,
      env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
      processObserver: { trackChild() {} } as never,
      hostGeneration: "v2",
      model: "openai/mock-model",
    });
    expect((await client.wait()).exit_code).toBe(0);
    const args = (await readFile(argumentsPath, "utf8")).trim().split("\n");
    expect(args).toContain("--print-logs");
    expect(args.slice(args.indexOf("--model"), args.indexOf("--model") + 2)).toEqual([
      "--model",
      "openai/mock-model",
    ]);
  });

  test("scenario tool aliases use the registered host names", () => {
    expect(hostToolName("ast_search")).toBe("ast_grep_search");
    expect(hostToolName("ast_replace")).toBe("ast_grep_replace");
    expect(hostToolName("read")).toBe("read");
    expect(hostToolArguments("read", { filePath: "sample.txt", limit: 10 })).toEqual({
      path: "sample.txt",
      limit: 10,
    });
  });

  test("tool result lookup follows the pinned host's 40-character call ids", () => {
    const callId = "callgraph-t6-callgraph-impact-payload-sites-incomplete";
    const exchanges = [
      {
        index: 0,
        label: "turn",
        request: {
          messages: [{ role: "tool", tool_call_id: callId.slice(0, 40), content: "done" }],
        },
        response: {},
        observed_at: "now",
      },
    ];
    expect(toolResultForCall(exchanges, callId)?.text).toBe("done");
  });

  test("title generation is excluded from scripted scenario turns", () => {
    const request = (system: string) => ({
      messages: [
        { role: "system", content: system },
        { role: "user", content: "control" },
      ],
    });
    expect(isTitleGenerationRequest(request("You are a title generator"))).toBe(true);
    expect(isTitleGenerationRequest(request("You are a coding agent"))).toBe(false);
  });

  test("turn-log liveness rejects a missing later turn", () => {
    expect(() => assertTurnLog(["tool", "result", "final"], ["tool", "result"])).toThrow(
      "turn_log_incomplete",
    );
  });

  test("truncation trailers accept lower-bound totals", () => {
    const match = new RegExp(TRUNCATION_TRAILER_PATTERN).exec(
      "shown 5 of ≥140 files (cap) · narrow: path",
    );
    expect(match?.groups).toMatchObject({ shown: "5", total: "140", reason: "cap" });
  });

  test("T6 requires the product-rendered trailer line byte for byte", () => {
    const fixture = scenario(call({ id: "t6", name: "glob", arguments: {} }));
    fixture.id = "glob/T6/incomplete";
    fixture.tool = "glob";
    fixture.trajectory = "T6";
    fixture.metadata = {
      t6: {
        fixture: "incomplete",
        call_id: "t6",
        triggered_reason: "cap",
        expected_trailer: "shown 5 of ≥140 files (cap) · narrow: path",
      },
    };
    expect(() =>
      assertT6Trailer(
        fixture,
        fixture.turns[0].response.kind === "tool_calls"
          ? fixture.turns[0].response.calls[0]
          : call(),
        "files\nshown 5 of ≥140 files (cap) · narrow: path",
      ),
    ).not.toThrow();
    expect(() =>
      assertT6Trailer(
        fixture,
        fixture.turns[0].response.kind === "tool_calls"
          ? fixture.turns[0].response.calls[0]
          : call(),
        "files\nshown 5 of ≥141 files (cap) · narrow: path",
      ),
    ).toThrow("does not exactly equal");
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
      request_body_flag: "--data",
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

  test("a control body is passed on the flag the captured contract names", async () => {
    const parent = await root();
    const executable = join(parent, "capture-body-arguments");
    const argumentsPath = join(parent, "body-arguments.txt");
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
      request_body_flag: "--data",
      shared_server_smoke: { method: "GET", path: "/api/health" },
    };

    await runApiControl(
      {
        id: "body-control",
        after_turn: "turn",
        method: "POST",
        path: "/api/session/one/permission/two/reply",
        body: { decision: "once" },
        purpose: "permission",
      },
      {
        executable,
        cwd: parent,
        env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
        contract,
        endpoint: "http://127.0.0.1:4096",
      },
    );

    // A body sent on a flag the host does not recognise never reaches it: the
    // command exits 1 before making the request, so the control answers
    // nothing.
    expect((await readFile(argumentsPath, "utf8")).trim().split("\n")).toEqual([
      "api",
      "--server",
      "http://127.0.0.1:4096",
      "POST",
      "/api/session/one/permission/two/reply",
      "--data",
      '{"decision":"once"}',
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
      { tool: "status", trajectories: cells },
      { tool: "powershell", trajectories: powershell },
    ];
    const projection = ["read", ...projectedControls];
    const schemas = { read: {}, powershell: {}, status: {} };
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
    const inventory = [...new Set([...projection, "powershell", "status"])].sort();
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

  test("an observation-only subset validates against the schema projection; a full run reads it as drift", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests", "docker", "opencode2", "scenarios", "read")),
    );
    const subset = await validateHarnessInputs({
      repoRoot: repo,
      scenarios,
      pinnedHostVersion: "0.0.0-beta-test",
      platform: "linux",
      observationOnly: true,
    });
    expect(subset.matrix).toBeDefined();
    expect(subset.inventory).toContain("read");
    expect(subset.inventory).toContain("apply_patch");
    // powershell stays in the inventory as a matrix row; on Linux every one of
    // its cells is n/a:platform and no scenario exists for it.
    const powershell = subset.matrix?.rows.find((row) => row.tool === "powershell");
    expect(powershell?.trajectories.T1).toBe("n/a:platform");

    const error = await expectCode(
      () =>
        validateHarnessInputs({
          repoRoot: repo,
          scenarios,
          pinnedHostVersion: "0.0.0-beta-test",
          platform: "linux",
          fullRun: true,
        }),
      "matrix_invalid",
    );
    // A subset on a full run is read as drift at the first guard it reaches:
    // the scenario-derived projection no longer matches the exclusion table.
    expect(error.message).toContain("do not match the explicit exclusion table");
  });

  test("callgraph scenarios retry after the background store warms", () => {
    const input = scenario(call({ name: "aft_callgraph" }));
    input.tool = "callgraph";
    const warmed = addCallgraphWarmup(input);
    expect(warmed.expected_turns).toEqual(["turn-1-warmup", "turn-1"]);
    expect(warmed.turns[0].response).toMatchObject({
      calls: [{ id: "warmup-call-1", name: "aft_callgraph" }],
    });
    expect(warmed.turns[1].delay_ms).toBe(2_000);
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
    expect(surfaces.find((surface) => surface.id === "grep..payload.matches")).toMatchObject({
      unit: "rows",
      narrow: ["path", "include", "exclude"],
    });
    const matrix = JSON.parse(
      await readFile(join(repo, "tests/docker/opencode2/matrix/applicability.json"), "utf8"),
    );
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests/docker/opencode2/scenarios")),
    );
    const incompatibleGlob = scenarios.find(
      (candidate) => candidate.id === "glob/T6/glob-payload-files/incomplete",
    );
    if (!incompatibleGlob) throw new Error("incomplete glob fixture missing");
    incompatibleGlob.metadata = structuredClone(incompatibleGlob.metadata);
    (incompatibleGlob.metadata?.t6 as Record<string, unknown>).expected_trailer =
      "shown 5 of ≥140 files (cap) · narrow: include";
    expect(() => validateT6(matrix, scenarios, surfaces)).toThrow(
      "expected trailer disagrees with the registry",
    );
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
  test("completion wake requires one host-transcript steer request", () => {
    const completionScenario = scenario(call());
    completionScenario.id = "bash/T5/completion_wake";
    completionScenario.tool = "bash";
    completionScenario.trajectory = "T5";
    completionScenario.execution = "shared-server";
    completionScenario.metadata = {
      t5: { source_call_id: "source", wake_turn: "wake-observed" },
    };
    const exchange = {
      index: 0,
      label: "wake-observed",
      request: {
        messages: [
          {
            role: "user",
            content: "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task bash-control\n</system-reminder>",
          },
        ],
      },
      response: {},
      observed_at: "now",
    };
    expect(() =>
      assertT5HostWakeTranscript(completionScenario, [exchange], {
        "task_id:source": "bash-control",
      }),
    ).not.toThrow();
    expect(() =>
      assertT5HostWakeTranscript(completionScenario, [], {
        "task_id:source": "bash-control",
      }),
    ).toThrow("host transcript contains 0 completion steer wakes");
  });

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
  test("concurrent-scenario-root-isolation", async () => {
    const firstRoot = await root();
    const secondRoot = await root();
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
      first.checkpointCall("call-1", "tool-result", "result"),
      second.checkpointCall("call-1", "tool-result", "result"),
    ]);
    expect(first.failures).toEqual([]);
    expect(second.failures).toEqual([]);
  });

  // Concurrent scenarios each own a root and an observer. A write that lands
  // in another scenario's root is reported by THAT root's observer under the
  // victim's scenario id: per-root observation cannot know the writer, only
  // where the effect landed, and two observers must never blur.
  test("cross-scenario-root-write-attributed-to-victim-root", async () => {
    const writerRoot = await root();
    const victimRoot = await root();
    const writer = new DiskStateObserver(writerRoot, "bash/T1/cross-root-writer");
    const victim = new DiskStateObserver(victimRoot, "read/T1/cross-root-victim");
    await Promise.all([writer.beginCall(call({ name: "bash" })), victim.beginCall(call())]);

    await writeFile(join(victimRoot, "escaped.txt"), "cross-root effect\n");
    await writer.checkpointCall("call-1", "tool-result", "result");
    const error = await expectCode(
      () => victim.checkpointCall("call-1", "tool-result", "result"),
      "undeclared_disk_effect",
    );
    expect(writer.failures).toEqual([]);
    expect(error.details).toMatchObject({
      scenario: "read/T1/cross-root-victim",
      originating_call: "call-1",
      path: "escaped.txt",
    });
  });

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

test("provider contract supplies the observed run model and config key to the harness", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-provider-config.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      provider_config: { openai: {} },
      opencode_json: { providers: { openai: {} } },
      run_command: ["opencode2", "run", "--model", "openai/mock-model", "message"],
    }),
  );

  const contract = await loadHostProviderConfigContract(contractRoot, "0.0.0-beta-test");
  expect(contract.model).toBe("openai/mock-model");
  expect(contract.config_key).toBe("providers");
});

test("a provider contract that does not show its config key is rejected", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-provider-config.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      provider_config: { openai: {} },
      run_command: ["opencode2", "run", "--model", "openai/mock-model", "message"],
    }),
  );
  await expectCode(
    () => loadHostProviderConfigContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
});

test("the V1 leg reads its own provider contract, under its own key", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host1-provider-config.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "1.18.30",
      observed_run_id: "probe-run",
      provider_config: { mock: { npm: "@ai-sdk/openai-compatible" } },
      opencode_json: { provider: { mock: { npm: "@ai-sdk/openai-compatible" } } },
      run_command: ["opencode", "run", "--model", "mock/mock-model", "message"],
    }),
  );

  const contract = await loadV1HostProviderConfigContract(contractRoot, "1.18.30");
  expect(contract.model).toBe("mock/mock-model");
  expect(contract.config_key).toBe("provider");
});

test("legacy host CLI capture names every field required by the runner", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-cli-contract.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      endpoint_handoff: { run_client_arguments: ["--server", "${endpoint}"] },
      password_handoff: { observed_run_client_environment: "OPENCODE_SERVER_PASSWORD" },
      session_start: {},
      idle_retention: {},
      shared_server_smoke: { positive_controls: [] },
    }),
  );
  const error = await expectCode(
    () => loadHostCliContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
  expect(error.details.missing_fields).toEqual([
    "endpoint_handoff.run",
    "endpoint_handoff.api",
    "password_handoff.run",
    "password_handoff.api",
    "request_body_handoff.api",
    "shared_server_smoke.method",
    "shared_server_smoke.path",
  ]);
});

test("transcript-shaped host schema capture satisfies validation", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-schema-rejection.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      observed_instance: { json_event_error: "Invalid arguments for tool read" },
      agent_visible_contract: { json_event: { type: "tool_use", state_status: "error" } },
    }),
  );
  const contract = await loadHostSchemaRejectionContract(
    contractRoot,
    "0.0.0-beta-test",
  );
  expect(contract.agent_visible_text).toBe("Invalid arguments for tool read");
  expect(contract.json_event).toEqual({ type: "tool_use", state_status: "error" });
});

test("missing host schema rejection contract fails validation instead of earning coverage", async () => {
  const contractRoot = await root();
  await expectCode(
    () => loadHostSchemaRejectionContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
});

describe("permission scenarios reach the host's own rules", () => {
  const permissionScenario = (
    id: string,
    tool: string,
    permission: Record<string, unknown>,
    calls: ToolCallPlan[],
  ): ScenarioDefinition => ({
    schema_version: 1,
    id,
    tool,
    trajectory: "T3",
    execution: "shared-server",
    prompt: "permission",
    turns: [{ label: "call-tool", response: { kind: "tool_calls", calls } }],
    metadata: { permission },
  });

  test("an ask scenario installs one ask rule for the operation it gates", () => {
    const rules = sessionPermissionRules(
      permissionScenario(
        "apply_patch/T3/apply_patch_ask_allow",
        "apply_patch",
        { operation: "apply_patch", reply: "once", requested_paths: ["patched.txt"] },
        [call({ id: "mutate", name: "apply_patch", arguments: {} })],
      ),
    );

    // The resource stays "*" because the tools disagree about how they name
    // one: a rule naming "patched.txt" matches the edit tool's relative path
    // but misses aft_delete's absolute path and bash's command line.
    expect(rules).toEqual([{ action: "edit", resource: "*", effect: "ask" }]);
  });

  test("every mutating operation asks under edit; read and bash ask under their own names", () => {
    expect(permissionAction("apply_patch")).toBe("edit");
    expect(permissionAction("write")).toBe("edit");
    expect(permissionAction("aft_move")).toBe("edit");
    expect(permissionAction("read")).toBe("read");
    expect(permissionAction("bash:withPermissionLoop")).toBe("bash");
    expect(permissionAction("bash:host-fallback")).toBe("bash");
  });

  test("a configured denial installs a deny rule and leaves nothing to answer", () => {
    const denial = permissionScenario(
      "read/T3/read_config_deny",
      "read",
      { operation: "read", reply: "config_deny", requested_path: "sample.txt" },
      [call({ id: "deny-call", name: "read", arguments: { filePath: "sample.txt" } })],
    );

    expect(sessionPermissionRules(denial)).toEqual([
      { action: "read", resource: "*", effect: "deny" },
    ]);
    expect(controlPlans(denial)).toEqual([]);
  });

  test("a scenario with no permission metadata installs no rules", () => {
    expect(sessionPermissionRules(scenario(call()))).toBeUndefined();
  });

  test("the approved and the rejected prompt are both read off the host's event stream", () => {
    const approved = permissionScenario(
      "apply_patch/T3/apply_patch_ask_allow",
      "apply_patch",
      { operation: "apply_patch", reply: "once", requested_paths: ["patched.txt"] },
      [call({ id: "mutate", name: "apply_patch", arguments: {} })],
    );
    const rejected = permissionScenario(
      "read/T3/read_ask_deny",
      "read",
      { operation: "read", reply: "reject", requested_path: "sample.txt" },
      [call({ id: "read-ask_deny", name: "read", arguments: { filePath: "sample.txt" } })],
    );
    const asked = (id: string, action: string, sourceId: string): HostEvent => ({
      type: "permission.asked",
      data: { id, action, resources: ["whatever the tool calls it"], source: { id: sourceId } },
    });
    const replied = (requestID: string, reply: string): HostEvent => ({
      type: "permission.replied",
      data: { requestID, reply },
    });

    assertPermissionPromptObserved(approved, [
      asked("per_1", "edit", "mutate"),
      replied("per_1", "once"),
    ]);
    assertPermissionPromptObserved(rejected, [
      asked("per_2", "read", "read-ask_deny"),
      replied("per_2", "reject"),
    ]);

    // A tool that completed without asking is the defect these rows exist to
    // catch, and it leaves no event behind.
    expect(() => assertPermissionPromptObserved(approved, [])).toThrow("no_effect_observed");
    // A prompt raised by some other call is not this row's prompt.
    expect(() =>
      assertPermissionPromptObserved(approved, [
        asked("per_3", "edit", "some-other-call"),
        replied("per_3", "once"),
      ]),
    ).toThrow("no_effect_observed");
    // Raised but answered the other way round is a different outcome than the
    // row declares.
    expect(() =>
      assertPermissionPromptObserved(approved, [
        asked("per_4", "edit", "mutate"),
        replied("per_4", "reject"),
      ]),
    ).toThrow("no_effect_observed");
  });

  test("a configured denial is expected to raise no prompt at all", () => {
    const denial = permissionScenario(
      "read/T3/read_config_deny",
      "read",
      { operation: "read", reply: "config_deny", requested_path: "sample.txt" },
      [call({ id: "deny-call", name: "read", arguments: { filePath: "sample.txt" } })],
    );

    assertPermissionPromptObserved(denial, []);
  });

  test("a permission scenario adds no control of its own", () => {
    expect(
      controlPlans(
        permissionScenario(
          "read/T3/read_ask_deny",
          "read",
          { operation: "read", reply: "reject", requested_path: "sample.txt" },
          [call({ id: "read-ask_deny", name: "read", arguments: { filePath: "sample.txt" } })],
        ),
      ),
    ).toEqual([]);
  });

  test("the shared server registers as a service and hands over that password", async () => {
    const parent = await root();
    const stateRoot = join(parent, "xdg-state");
    const argumentsPath = join(parent, "serve-arguments.txt");
    const executable = join(parent, "fake-opencode");
    await writeFile(
      executable,
      `#!/bin/sh\n` +
        `printf '%s\\n' "$@" > ${JSON.stringify(argumentsPath)}\n` +
        `mkdir -p "$XDG_STATE_HOME/opencode"\n` +
        `printf '%s' '{"id":"x","version":"2.0.11","url":"http://127.0.0.1:4242",` +
        `"pid":1,"password":"from-registration"}' > "$XDG_STATE_HOME/opencode/service.json"\n` +
        `echo "server listening on http://127.0.0.1:4242"\n` +
        `sleep 30\n`,
    );
    await chmod(executable, 0o755);

    const server = await startSharedServer({
      executable,
      cwd: parent,
      env: { ...process.env, XDG_STATE_HOME: stateRoot },
      processObserver: new ProcessObserver("registration-test"),
      stateRoot,
      timeoutMs: 10_000,
    });

    try {
      // `--service` is the flag that makes the host publish the registration
      // file `discover()` reads; a plain `serve` is invisible to the plugin
      // running inside it.
      expect((await readFile(argumentsPath, "utf8")).trim().split("\n")).toContain("--service");
      expect(server.endpoint).toBe("http://127.0.0.1:4242");
      expect(server.password).toBe("from-registration");
      expect(server.registration.pid).toBe(1);
    } finally {
      server.child.kill("SIGKILL");
    }
  });

  test("a server that listens without registering is rejected", async () => {
    const parent = await root();
    const executable = join(parent, "unregistered-opencode");
    await writeFile(
      executable,
      '#!/bin/sh\necho "server listening on http://127.0.0.1:4242"\nsleep 30\n',
    );
    await chmod(executable, 0o755);

    await expectCode(
      () =>
        startSharedServer({
          executable,
          cwd: parent,
          env: { ...process.env, XDG_STATE_HOME: join(parent, "xdg-state") },
          processObserver: new ProcessObserver("unregistered-test"),
          stateRoot: join(parent, "xdg-state"),
          timeoutMs: 2_000,
        }),
      "host_failed",
    );
  });
});

describe("a parity row compares the two hosts", () => {
  function parityScenario(): ScenarioDefinition {
    return {
      schema_version: 1,
      id: "write/T7/happy",
      tool: "write",
      trajectory: "T7",
      execution: "standalone",
      prompt: "parity",
      turns: [{ label: "finish", response: { kind: "text", content: "done" } }],
      compare_call_id: "call-1",
      comparison: {
        mode: "shape",
        rules: [
          { kind: "field", field: "write_outcome", pattern: "^(?<value>Created new file\\.)$" },
        ],
        expected: { write_outcome: "Created new file." },
      },
    };
  }

  async function allowlistRoot(entries: unknown[]): Promise<string> {
    const directory = await root();
    await writeFile(
      join(directory, "parity-allowlist.json"),
      JSON.stringify({ schema_version: 1, entries }),
    );
    return directory;
  }

  test("a field the two hosts may differ on is allowed by name", async () => {
    const scenario = parityScenario();
    scenario.comparison = {
      mode: "shape",
      rules: [
        { kind: "field", field: "write_outcome", pattern: "^(?<value>Created new file\\.)$" },
        { kind: "field", field: "elapsed", pattern: "^took (?<value>\\d+)ms$" },
      ],
      expected: { write_outcome: "Created new file.", elapsed: 1 },
    };
    const directory = await allowlistRoot([
      {
        scenario: "write/T7/happy",
        tool: "write",
        field: "elapsed",
        reason: "Wall clock is not a product output.",
      },
    ]);

    expect(await validateParityAllowlist(directory, [scenario])).toHaveLength(1);
  });

  test("an allowlist that drops every projected field is refused, because it compares nothing", async () => {
    const directory = await allowlistRoot([
      {
        scenario: "write/T7/happy",
        tool: "write",
        field: "write_outcome",
        reason: "Host transports add presentation text around the tool result.",
      },
    ]);

    await expect(validateParityAllowlist(directory, [parityScenario()])).rejects.toThrow(
      "parity allowlist leaves write/T7/happy comparing nothing between the hosts",
    );
  });
});

describe("the run reports a verdict for every row", () => {
  const matrix = {
    rows: [
      {
        tool: "alpha",
        trajectories: {
          T1: "applicable",
          T2: "applicable",
          T3: "expected_fail:https://example.invalid/1",
          T4: "expected_fail:https://example.invalid/1",
          T5: "n/a:no-background-capability",
          T6: "n/a:no-list-surface",
          T7: "n/a:no-list-surface",
        },
      },
    ],
  } as unknown as Parameters<typeof reportTable>[0];

  const result = (
    id: string,
    status: ScenarioResult["status"],
    failure?: ScenarioResult["failure"],
  ): ScenarioResult => ({ id, status, failure, forensic_dir: "/dev/null" });

  test("a failed row does not stop the rows after it, and the counts are stated", () => {
    const report = reportTable(matrix, [
      result("alpha/T1/happy", "failed", { code: "host_failed", message: "first row broke" }),
      result("alpha/T2/happy", "passed"),
      result("alpha/T3/gated", "failed", { code: "host_failed", message: "known upstream gap" }),
      result("alpha/T4/gated", "failed", { code: "host_failed", message: "known upstream gap" }),
    ]);

    expect(report.text).toContain("alpha | T1 | applicable | fail");
    expect(report.text).toContain("alpha | T2 | applicable | pass");
    expect(report.text).toContain("rows: 1 passed, 1 failed, 2 expected-failed, 3 not-applicable (of 7)");
    expect(report.text).toContain("scenarios: 1 passed, 3 failed (of 4)");
    expect(report.failed).toBe(true);
  });

  test("an expected-failure label cannot excuse a harness-integrity failure", () => {
    const productGap = result("alpha/T3/gated", "failed", {
      code: "host_failed",
      message: "known upstream gap",
    });
    const integrityBreach = result("alpha/T3/gated", "failed", {
      code: "executable_provenance",
      message: "producer sidecar missing",
      unsuppressible: true,
    });
    // An unsuppressible product outcome is still a product outcome: the label
    // is about the product, so it may cover this one.
    const unparsedOutput = result("alpha/T3/gated", "failed", {
      code: "projection_unparsed",
      message: "Created new file.",
      unsuppressible: true,
    });

    expect(parentDisposition("expected_fail:https://example.invalid/1", [productGap])).toBe(
      "expected_fail",
    );
    expect(parentDisposition("expected_fail:https://example.invalid/1", [integrityBreach])).toBe(
      "fail",
    );
    expect(parentDisposition("expected_fail:https://example.invalid/1", [unparsedOutput])).toBe(
      "expected_fail",
    );
  });
});
