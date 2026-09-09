#!/usr/bin/env bun
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { loadHostCliContract, loadHostProviderConfigContract } from "./contracts.js";
import { assertHarnessControlCoverage, runHarnessControlSuite } from "./control-suite.js";
import { DiskStateObserver, ThreeStateRecorder } from "./disk-state.js";
import { HarnessError } from "./errors.js";
import { loadHarnessExtensions } from "./extensions.js";
import { ScenarioForensics } from "./forensics.js";
import {
  runApiCommand,
  runApiControl,
  runSharedServerSmoke,
  startScenarioClient,
  startSharedServer,
  type ControlPathValues,
  type SharedServerHandle,
} from "./host.js";
import { createScenarioIsolation, assertPluginLoadEvidence } from "./isolation.js";
import { DeterministicScenarioMock, toolCallsInTurn, toolResultForCall } from "./mock-server.js";
import { readPinnedHostVersion } from "./pin.js";
import { ProcessObserver } from "./process-observer.js";
import { AftTaskProbe } from "./task-probe.js";
import { assertComparison, projectText, TRUNCATION_TRAILER_PATTERN } from "./projection.js";
import { verifyExecutableProvenance } from "./provenance.js";
import { filterScenarios, loadScenarios, materializeParityScenarios } from "./scenario-loader.js";
import {
  HOST_SCHEMA_REJECTION_PROBE,
  writeHostSchemaRejectionObservation,
} from "./schema-observation.js";
import { assertTurnLog, readTurnLog } from "./turn-log.js";
import type {
  ApiControlPlan,
  HarnessRuntimeEvent,
  ScenarioDefinition,
  ScenarioLifecycleContext,
  ScenarioResult,
  ToolCallPlan,
} from "./types.js";
import { TRAJECTORIES } from "./types.js";
import {
  type ApplicabilityMatrix,
  type MatrixClassification,
  type ParityAllowlistEntry,
  validateHarnessInputs,
} from "./validation.js";
import { asRecord, createRunId } from "./util.js";

const harnessRoot = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(harnessRoot, "../../../..");

interface DriverConfig {
  executable: string;
  hostExecutable: string;
  v1HostExecutable?: string;
  pluginTarball: string;
  runRoot: string;
  selector?: string;
  validateOnly: boolean;
  captureSchemaObservation?: string;
  checkoutSha?: string;
}

function argumentValue(name: string): string | undefined {
  const exact = process.argv.findIndex((argument) => argument === name);
  if (exact !== -1) return process.argv[exact + 1];
  return process.argv.find((argument) => argument.startsWith(`${name}=`))?.slice(name.length + 1);
}

function requiredEnvironment(name: string, fallback?: string): string {
  const value = process.env[name] ?? fallback;
  if (!value) throw new Error(`${name} is required`);
  return value;
}

async function configuration(): Promise<DriverConfig> {
  const runId = process.env.AFT_E2E_RUN_ID ?? createRunId();
  const runRoot = resolve(
    process.env.AFT_E2E_RUN_ROOT ?? join(process.env.TMPDIR ?? "/tmp", "aft-opencode2", runId),
  );
  return {
    executable: resolve(requiredEnvironment("AFT_BINARY_PATH")),
    hostExecutable: resolve(requiredEnvironment("OPENCODE2_BIN")),
    v1HostExecutable: process.env.OPENCODE1_BIN ? resolve(process.env.OPENCODE1_BIN) : undefined,
    pluginTarball: resolve(requiredEnvironment("AFT_OPENCODE2_PLUGIN_TARBALL")),
    runRoot,
    selector: process.env.AFT_E2E_SCENARIO ?? argumentValue("--scenario"),
    validateOnly: process.argv.includes("--validate-only"),
    captureSchemaObservation:
      process.env.AFT_OPENCODE2_SCHEMA_OBSERVATION ?? argumentValue("--capture-schema-observation"),
    checkoutSha: process.env.AFT_CHECKOUT_SHA,
  };
}

function fixturePath(scenario: ScenarioDefinition): string | undefined {
  if (!scenario.fixture) return undefined;
  if (scenario.fixture.startsWith("/")) return scenario.fixture;
  const registrationDirectory = scenario.registration_path
    ? dirname(scenario.registration_path)
    : repoRoot;
  return resolve(registrationDirectory, scenario.fixture);
}

function plannedCalls(scenario: ScenarioDefinition): ToolCallPlan[] {
  return scenario.turns.flatMap(toolCallsInTurn);
}

function scenarioToolName(name: string): string {
  const bare = name.startsWith("aft_") ? name.slice(4) : name;
  if (bare === "ast_grep_search") return "ast_search";
  if (bare === "ast_grep_replace") return "ast_replace";
  return bare;
}

function controlPlans(scenario: ScenarioDefinition): ApiControlPlan[] {
  const controls = [...(scenario.controls ?? [])];
  const permission = asRecord(scenario.metadata?.permission);
  const reply = permission?.reply;
  if (
    scenario.trajectory !== "T3" ||
    (reply !== "once" && reply !== "reject") ||
    controls.some((control) => control.purpose === "permission")
  ) {
    return controls;
  }
  const subject = scenario.turns.find((turn) => {
    if (turn.response.kind !== "tool_calls") return false;
    return turn.response.calls.some((call) => scenarioToolName(call.name) === scenario.tool);
  });
  const subjectCall = subject?.response.kind === "tool_calls"
    ? subject.response.calls.find((call) => scenarioToolName(call.name) === scenario.tool)
    : undefined;
  if (!subject || !subjectCall) {
    fail("scenario_invalid", `${scenario.id}: permission scenario has no subject tool call`);
  }
  const id = `${scenario.id.replaceAll("/", "-")}-permission`;
  if (controls.some((control) => control.id === id)) return controls;
  controls.push({
    id,
    after_turn: subject.label,
    delay_ms: 100,
    method: "POST",
    path: `/api/session/{{session_id}}/permission/{{permission_id:${subjectCall.id}}}/reply`,
    body: { reply },
    expected_status: 0,
    purpose: "permission",
  });
  return controls;
}

function observeControlPathValues(
  value: unknown,
  values: Record<string, string>,
  sourceCallId?: string,
): void {
  if (Array.isArray(value)) {
    for (const item of value) observeControlPathValues(item, values, sourceCallId);
    return;
  }
  const record = asRecord(value);
  if (!record) return;
  const sessionId = record.sessionID ?? record.session_id;
  if (typeof sessionId === "string") values.session_id = sessionId;
  const permissionId = record.permissionID ?? record.permission_id;
  if (typeof permissionId === "string") values.permission_id = permissionId;
  const taskId = record.taskID ?? record.taskId ?? record.task_id;
  if (typeof taskId === "string") {
    values.task_id = taskId;
    if (sourceCallId) values[`task_id:${sourceCallId}`] = taskId;
  }
  if (typeof record.type === "string" && record.type.includes("permission") && typeof record.id === "string") {
    values.permission_id = record.id;
  }
  for (const nested of Object.values(record)) {
    if (nested !== value) observeControlPathValues(nested, values, sourceCallId);
  }
}

function observeTaskId(text: string, callId: string, values: Record<string, string>): void {
  const taskId = text.match(/\b(?:bash|task)-[A-Za-z0-9_-]+\b/)?.[0];
  if (!taskId) return;
  values.task_id = taskId;
  values[`task_id:${callId}`] = taskId;
}

function observeHostStream(
  child: import("node:child_process").ChildProcess,
  values: Record<string, string>,
): void {
  let pending = "";
  child.stdout?.on("data", (chunk) => {
    pending += String(chunk);
    const lines = pending.split("\n");
    pending = lines.pop() ?? "";
    for (const line of lines) {
      try {
        observeControlPathValues(JSON.parse(line), values);
      } catch {}
    }
  });
}

async function discoverPermissionRequestId(
  control: ApiControlPlan,
  values: Record<string, string>,
  options: Omit<Parameters<typeof runApiCommand>[0], "method" | "path" | "body">,
): Promise<void> {
  const placeholder = control.path.match(/\{\{permission_id(?::([^{}]+))?\}\}/);
  if (!placeholder) return;
  const qualifier = placeholder[1];
  const key = qualifier ? `permission_id:${qualifier}` : "permission_id";
  if (values[key]) return;

  const deadline = Date.now() + 5_000;
  while (!values.session_id && Date.now() < deadline) await Bun.sleep(25);
  const sessionId = values.session_id;
  if (!sessionId) {
    fail("host_failed", `${control.id}: active session id was not observed before permission reply`);
  }
  while (Date.now() < deadline) {
    const result = await runApiCommand({
      ...options,
      method: "GET",
      path: `/api/session/${encodeURIComponent(sessionId)}/permission`,
      timeoutMs: 2_000,
    });
    if (result.exit_code === 0) {
      try {
        const data = asRecord(JSON.parse(result.stdout))?.data;
        if (Array.isArray(data)) {
          const requests = data.map(asRecord).filter((request) => request !== undefined);
          const matching = qualifier
            ? requests.find((request) => asRecord(request.source)?.id === qualifier)
            : requests.at(-1);
          if (typeof matching?.id === "string") {
            values.permission_id = matching.id;
            values[key] = matching.id;
            return;
          }
        }
      } catch {}
    }
    await Bun.sleep(50);
  }
  fail("host_failed", `${control.id}: pending permission request id was not observed`);
}

function restoreCallSequence(scenario: ScenarioDefinition): ToolCallPlan[] {
  if (!scenario.restore_evidence) return [];
  const available = [...plannedCalls(scenario)];
  return scenario.restore_evidence.calls.map((expected) => {
    const index = available.findIndex(
      (call) =>
        call.name === expected.tool &&
        JSON.stringify(call.arguments) === JSON.stringify(expected.arguments),
    );
    if (index === -1) {
      throw new Error(
        `${scenario.id}: restore evidence call not found: ${expected.tool} ${JSON.stringify(expected.arguments)}`,
      );
    }
    return available.splice(index, 1)[0];
  });
}

function assertT2ProductContract(
  scenario: ScenarioDefinition,
  call: ToolCallPlan,
  text: string,
  hostStream: string,
): void {
  if (
    scenario.trajectory !== "T2" ||
    scenario.error_origin !== "product" ||
    scenario.compare_call_id !== call.id
  ) {
    return;
  }
  const code = scenario.metadata?.error_code;
  const steeringPattern = scenario.metadata?.steering_pattern;
  if (typeof code !== "string" || !hostStream.includes(code)) {
    throw new Error(
      `${scenario.id}: JSON event stream does not contain product error code ${String(code)}`,
    );
  }
  if (typeof steeringPattern !== "string" || !new RegExp(steeringPattern).test(text)) {
    throw new Error(`${scenario.id}: agent-visible text does not match steering pattern`);
  }
}

function assertT6Trailer(scenario: ScenarioDefinition, call: ToolCallPlan, text: string): void {
  const t6 = asRecord(scenario.metadata?.t6);
  if (!t6) return;
  const subjectCall = t6.call_id ?? scenario.compare_call_id;
  if (subjectCall !== call.id) return;
  const matches = [...text.matchAll(new RegExp(TRUNCATION_TRAILER_PATTERN, "gm"))];
  if (t6.fixture === "complete" && matches.length !== 0) {
    throw new Error(`${scenario.id}: complete T6 fixture rendered a truncation trailer`);
  }
  if (t6.fixture === "incomplete") {
    if (matches.length !== 1) {
      throw new Error(`${scenario.id}: incomplete T6 fixture rendered ${matches.length} trailers`);
    }
    if (matches[0].groups?.reason !== t6.triggered_reason) {
      throw new Error(
        `${scenario.id}: trailer reason ${String(matches[0].groups?.reason)} does not equal ${String(
          t6.triggered_reason,
        )}`,
      );
    }
  }
}

function errorRecord(error: unknown): ScenarioResult["failure"] {
  if (error instanceof HarnessError) {
    return { code: error.code, message: error.message, details: { ...error.details } };
  }
  return {
    code: error instanceof Error ? error.name : "NonError",
    message: error instanceof Error ? error.message : String(error),
  };
}

async function makeTransportDeadStub(
  isolationRoot: string,
  configuredExecutable: string,
): Promise<string> {
  const stub = join(isolationRoot, ".harness-state", "aft-transport-dead");
  await mkdir(dirname(stub), { recursive: true });
  await writeFile(
    stub,
    `#!/bin/sh\nif [ "\${1:-}" = "--version" ] || [ "\${1:-}" = "-V" ]; then exec ${JSON.stringify(
      configuredExecutable,
    )} "$@"; fi\necho "AFT harness transport unavailable" >&2\nexit 3\n`,
  );
  await chmod(stub, 0o755);
  return stub;
}

async function readPackageVersion(): Promise<string> {
  const manifest = JSON.parse(
    await readFile(join(repoRoot, "packages", "opencode-plugin", "package.json"), "utf8"),
  ) as { version?: string };
  if (!manifest.version) throw new Error("opencode plugin package version is missing");
  return manifest.version;
}

async function runOneScenario(options: {
  scenario: ScenarioDefinition;
  config: DriverConfig;
  pinnedHostVersion: string;
  pluginVersion: string;
  hostContract?: Awaited<ReturnType<typeof loadHostCliContract>>;
  extensions: Awaited<ReturnType<typeof loadHarnessExtensions>>;
  runSmoke: boolean;
  hostGeneration?: "v1" | "v2";
  hostExecutable?: string;
  applyComparison?: boolean;
  providerConfig?: Record<string, unknown>;
}): Promise<{
  result: ScenarioResult;
  smokeRan: boolean;
  observedTexts: Record<string, string>;
}> {
  const { scenario, config } = options;
  const hostGeneration = options.hostGeneration ?? "v2";
  const hostExecutable = options.hostExecutable ?? config.hostExecutable;
  const forensicId =
    scenario.trajectory === "T7" ? `${scenario.id}/${hostGeneration}` : scenario.id;
  const forensics = new ScenarioForensics(config.runRoot, forensicId);
  await forensics.initialize(scenario);
  const turnLogPath = join(forensics.directory, "turns.log");
  let disk: DiskStateObserver | undefined;
  let processObserver: ProcessObserver | undefined;
  let threeState: ThreeStateRecorder | undefined;
  let server: SharedServerHandle | undefined;
  let isolation: Awaited<ReturnType<typeof createScenarioIsolation>> | undefined;
  let lifecycleContext: ScenarioLifecycleContext | undefined;
  let hostStream = "";
  let pluginLog = "";
  let mock: DeterministicScenarioMock | undefined;
  let firstFailure: unknown;
  let smokeRan = false;
  const begun = new Set<string>();
  const resultObserved = new Set<string>();
  const controlPromises: Promise<unknown>[] = [];
  const observedTexts: Record<string, string> = {};
  let restoreCalls: ToolCallPlan[] = [];
  let abortIssuedAt: number | undefined;
  let hostCompletedAt: number | undefined;
  const controlPathValues: Record<string, string> = {};

  const recordFailure = (error: unknown) => {
    firstFailure ??= error;
  };

  const emit = async (event: HarnessRuntimeEvent) => {
    if (!lifecycleContext) return;
    for (const extension of options.extensions) {
      try {
        await extension.observe?.(lifecycleContext, event);
      } catch (error) {
        recordFailure(error);
      }
    }
  };

  const captureRestoreResult = async (call: ToolCallPlan) => {
    if (!threeState) return;
    const index = restoreCalls.findIndex((candidate) => candidate.id === call.id);
    if (index === -1) return;
    if (restoreCalls.length === 2) {
      await threeState.capture(index === 0 ? "intermediate" : "restored");
      return;
    }
    if (index === 0) await threeState.capture("before");
    else if (index === restoreCalls.length - 2) await threeState.capture("intermediate");
    else if (index === restoreCalls.length - 1) await threeState.capture("restored");
  };

  try {
    mock = new DeterministicScenarioMock(scenario, turnLogPath, {
      beforeTurn: async (turn, request) => {
        observeControlPathValues(request, controlPathValues);
        if (!disk) return;
        const pseudoExchange = {
          index: -1,
          label: "request-observation",
          request,
          response: {},
          observed_at: new Date().toISOString(),
        };
        for (const call of plannedCalls(scenario)) {
          if (!begun.has(call.id) || resultObserved.has(call.id)) continue;
          const observed = toolResultForCall([pseudoExchange], call.id);
          if (!observed) continue;
          observeControlPathValues(observed.event, controlPathValues, call.id);
          observeTaskId(observed.text, call.id, controlPathValues);
          await emit({
            kind: "tool_result_observed",
            call,
            before_turn: turn.label,
            at: Date.now(),
          });
          try {
            await disk.checkpointCall(call.id, `mock-result-before-${turn.label}`, "result");
            resultObserved.add(call.id);
          } catch (error) {
            recordFailure(error);
          }
          try {
            await captureRestoreResult(call);
          } catch (error) {
            recordFailure(error);
          }
        }
        for (const call of toolCallsInTurn(turn)) {
          await emit({ kind: "tool_call_accepted", call, turn: turn.label, at: Date.now() });
          try {
            if (threeState && restoreCalls.length === 2 && call.id === restoreCalls[0]?.id) {
              await threeState.capture("before");
            }
            await disk.beginCall(call);
            begun.add(call.id);
          } catch (error) {
            recordFailure(error);
          }
        }
      },
      afterRequest: async (exchange) => {
        await emit({ kind: "mock_exchange", exchange });
        if (!server || !isolation || !options.hostContract) return;
        const controlServer = server;
        const controlIsolation = isolation;
        const controlContract = options.hostContract;
        for (const control of controlPlans(scenario).filter(
          (candidate) => candidate.after_turn === exchange.label,
        )) {
          const controlPromise = (async () => {
            if (control.delay_ms) await Bun.sleep(control.delay_ms);
            const apiOptions = {
              executable: hostExecutable,
              cwd: controlIsolation.project,
              env: controlIsolation.env,
              contract: controlContract,
              endpoint: controlServer.endpoint,
              password: controlServer.password,
            };
            await discoverPermissionRequestId(control, controlPathValues, apiOptions);
            await emit({ kind: "control_started", control, at: Date.now() });
            if (control.purpose === "abort") abortIssuedAt ??= Date.now();
            const output = await runApiControl(
              { ...control, delay_ms: 0 },
              {
                ...apiOptions,
                controlPathValues: controlPathValues as ControlPathValues,
              },
            );
            await forensics.writeJson(`control-${control.id}.json`, output);
            await emit({ kind: "control_completed", control, at: Date.now(), output });
            if (control.purpose === "abort" && disk) {
              for (const call of plannedCalls(scenario).filter(
                (candidate) => candidate.outlives_result && begun.has(candidate.id),
              )) {
                await disk.checkpointCall(call.id, `control-${control.id}`, "cancellation");
              }
            }
          })().catch(recordFailure);
          controlPromises.push(controlPromise);
        }
      },
    });
    await mock.start();
    isolation = await createScenarioIsolation({
      parent: join(config.runRoot, "scenarios"),
      scenarioId: scenario.id,
      fixture: fixturePath(scenario),
      pluginTarball: config.pluginTarball,
      mockBaseUrl: mock.url,
      model: (scenario.model ?? "mock/mock-model").split("/").at(-1),
      projectConfig: scenario.project_config,
      providerConfig: hostGeneration === "v2" ? options.providerConfig : undefined,
    });
    if (scenario.id.startsWith("bash/T3/fallback_")) {
      isolation.env.AFT_BINARY_PATH = await makeTransportDeadStub(
        isolation.root,
        config.executable,
      );
    } else {
      isolation.env.AFT_BINARY_PATH = config.executable;
    }
    disk = new DiskStateObserver(isolation.project, scenario.id);
    restoreCalls = restoreCallSequence(scenario);
    if (scenario.restore_evidence) {
      threeState = new ThreeStateRecorder(isolation.project, scenario.restore_evidence.paths);
    }
    processObserver = new ProcessObserver(
      scenario.id,
      new AftTaskProbe(join(isolation.data, "cortexkit", "aft", "aft.db")),
    );
    lifecycleContext = {
      scenario,
      run_root: config.runRoot,
      project_root: isolation.project,
      forensic_dir: forensics.directory,
      host_generation: hostGeneration,
    };
    for (const extension of options.extensions) await extension.beforeScenario?.(lifecycleContext);

    if (scenario.execution === "shared-server") {
      server = await startSharedServer({
        executable: hostExecutable,
        cwd: isolation.project,
        env: isolation.env,
        processObserver,
      });
    }
    const client = startScenarioClient({
      executable: hostExecutable,
      scenario,
      cwd: isolation.project,
      env: isolation.env,
      processObserver,
      contract: options.hostContract,
      server,
      hostGeneration,
    });
    observeHostStream(client.child, controlPathValues);
    if (options.runSmoke && server && options.hostContract) {
      const smoke = await runSharedServerSmoke({
        executable: hostExecutable,
        cwd: isolation.project,
        env: isolation.env,
        contract: options.hostContract,
        server,
      });
      await forensics.writeJson("shared-server-smoke.json", smoke);
      smokeRan = true;
    }
    const host = await client.wait(
      typeof scenario.metadata?.host_timeout_ms === "number"
        ? scenario.metadata.host_timeout_ms
        : 120_000,
    );
    hostCompletedAt = Date.now();
    await emit({ kind: "host_exit", at: hostCompletedAt, output: host });
    hostStream = host.stdout;
    await forensics.writeJson("host-command.json", host);
    await forensics.writeText("host-stream.ndjson", host.stdout);
    await forensics.writeText("host-stderr.log", host.stderr);
    const acceptedExitCodes = Array.isArray(scenario.metadata?.accepted_exit_codes)
      ? scenario.metadata.accepted_exit_codes
      : [0];
    if (!acceptedExitCodes.includes(host.exit_code)) {
      throw new Error(`host exited ${host.exit_code}; accepted ${acceptedExitCodes.join(",")}`);
    }
    await Promise.all(controlPromises);
    if (
      scenario.trajectory === "T4" &&
      (abortIssuedAt === undefined || hostCompletedAt - abortIssuedAt > 5_000)
    ) {
      throw new Error(
        `${scenario.id}: interruption exceeded 5s (${String(
          abortIssuedAt === undefined ? "abort not issued" : hostCompletedAt - abortIssuedAt,
        )})`,
      );
    }
    if (firstFailure) throw firstFailure;

    for (const call of plannedCalls(scenario)) {
      const observed = toolResultForCall(mock.exchanges, call.id);
      if (!observed) throw new Error(`mock never observed tool result for ${call.id}`);
      observedTexts[call.id] = observed.text;
      assertT2ProductContract(scenario, call, observed.text, hostStream);
      assertT6Trailer(scenario, call, observed.text);
      if (
        options.applyComparison !== false &&
        scenario.compare_call_id === call.id &&
        scenario.comparison
      ) {
        assertComparison(observed.text, scenario.comparison);
      }
    }
    threeState?.assertComplete();
    const expectedTurns = scenario.expected_turns ?? scenario.turns.map((turn) => turn.label);
    assertTurnLog(expectedTurns, await readTurnLog(turnLogPath));
    await forensics.writeExchanges(mock.exchanges);

    pluginLog = await readFile(isolation.plugin_log, "utf8");
    await forensics.writeText("plugin.log", pluginLog);
    assertPluginLoadEvidence({
      log: pluginLog,
      pluginTarball: config.pluginTarball,
      expectedVersion: options.pluginVersion,
      expectedEntry: hostGeneration === "v1" ? "root" : "server",
    });
    if (options.config.captureSchemaObservation && scenario.error_origin === "host") {
      await writeHostSchemaRejectionObservation({
        outputPath: resolve(options.config.captureSchemaObservation),
        hostVersion: options.pinnedHostVersion,
        runId: config.runRoot.split("/").at(-1) ?? "unknown",
        scenarioId: scenario.id,
        hostStream,
        exchanges: mock.exchanges,
      });
    }
  } catch (error) {
    recordFailure(error);
  } finally {
    await Promise.allSettled(controlPromises);
    try {
      await mock?.stop();
    } catch (error) {
      recordFailure(error);
    }
    if (processObserver) {
      const termination = await processObserver.cleanupAndConfirm(
        scenario.quiescence_timeout_ms ?? 30_000,
      );
      await forensics.writeJson("termination.json", termination);
      await emit({ kind: "termination", at: Date.now(), evidence: termination });
      if (scenario.id.startsWith("bash/T3/loop_")) {
        const permissionRequired = pluginLog.indexOf("permission_required");
        const retry = pluginLog.indexOf("retry", permissionRequired + 1);
        if (permissionRequired === -1 || retry === -1 || termination.tasks.length === 0) {
          recordFailure(
            new Error(
              `${scenario.id}: loop identity lacks permission_required -> retry or AFT task row`,
            ),
          );
        }
      }
      if (scenario.id.startsWith("bash/T3/fallback_")) {
        const fallbackText = plannedCalls(scenario)
          .map((call) => toolResultForCall(mock?.exchanges ?? [], call.id)?.text ?? "")
          .join("\n");
        if (!fallbackText.includes("AFT UNAVAILABLE") || termination.tasks.length !== 0) {
          recordFailure(
            new Error(
              `${scenario.id}: fallback identity requires AFT UNAVAILABLE and no AFT task row`,
            ),
          );
        }
      }
      if (
        scenario.trajectory === "T4" &&
        !termination.tasks.some((task) => task.status_reason === "call_aborted")
      ) {
        recordFailure(
          new Error(`${scenario.id}: Rust task row did not record status_reason call_aborted`),
        );
      }
      if (disk) {
        for (const call of plannedCalls(scenario).filter(
          (candidate) => candidate.outlives_result && begun.has(candidate.id),
        )) {
          try {
            disk.markTerminal(
              call.id,
              termination.tasks.map((task) => task.id),
              [...new Set(termination.processes.map((processRecord) => processRecord.pgid))],
            );
            await disk.checkpointCall(call.id, "terminal-after-confirmed-stop", "post_result");
          } catch (error) {
            recordFailure(error);
          }
        }
        try {
          await disk.finalize(termination.stopped);
        } catch (error) {
          recordFailure(error);
        }
        await forensics.writeJson("disk-checkpoints.json", disk.checkpoints);
        await forensics.writeJson("disk-failures.json", disk.failures);
        await emit({
          kind: "disk_checkpoints",
          at: Date.now(),
          checkpoints: disk.checkpoints,
        });
      }
    }
  }

  let result: ScenarioResult = firstFailure
    ? {
        id: scenario.id,
        status: "failed",
        issue: scenario.expected_fail_issue,
        failure: errorRecord(firstFailure),
        forensic_dir: forensics.directory,
      }
    : {
        id: scenario.id,
        status: "passed",
        issue: scenario.expected_fail_issue,
        forensic_dir: forensics.directory,
      };
  if (firstFailure) await forensics.recordFailure(firstFailure);
  if (lifecycleContext) {
    for (const extension of options.extensions) {
      try {
        await extension.afterScenario?.(lifecycleContext, result);
      } catch (error) {
        recordFailure(error);
        result = {
          id: scenario.id,
          status: "failed",
          issue: scenario.expected_fail_issue,
          failure: errorRecord(error),
          forensic_dir: forensics.directory,
        };
        await forensics.recordFailure(error);
      }
    }
  }
  return { result, smokeRan, observedTexts };
}

function assertDualHostParity(
  scenario: ScenarioDefinition,
  v1Text: string,
  v2Text: string,
  allowlist: readonly ParityAllowlistEntry[],
): void {
  if (!scenario.comparison) throw new Error(`${scenario.id}: T7 requires a declared comparison`);
  const allowedFields = allowlist
    .filter((entry) => entry.scenario === scenario.id)
    .map((entry) => entry.field);
  if (scenario.comparison.mode === "exact") {
    if (allowedFields.length > 0) {
      throw new Error(`${scenario.id}: exact parity cannot have field exceptions`);
    }
    if (v1Text !== v2Text) {
      throw new Error(`${scenario.id}: exact V1/V2 parity mismatch`);
    }
    return;
  }
  const v1Shape = projectText(v1Text, scenario.comparison.rules);
  const v2Shape = projectText(v2Text, scenario.comparison.rules);
  for (const field of allowedFields) {
    delete v1Shape[field];
    delete v2Shape[field];
  }
  if (JSON.stringify(v1Shape) !== JSON.stringify(v2Shape)) {
    throw new Error(
      `${scenario.id}: projected V1/V2 parity mismatch\nV1 ${JSON.stringify(v1Shape)}\nV2 ${JSON.stringify(v2Shape)}`,
    );
  }
}

function parentDisposition(
  classification: MatrixClassification,
  results: readonly ScenarioResult[],
): "expected_fail" | "fail" | "n/a" | "pass" {
  if (classification.startsWith("n/a:")) return "n/a";
  if (classification.startsWith("expected_fail:")) {
    return results.length > 0 && results.every((result) => result.status === "failed")
      ? "expected_fail"
      : "fail";
  }
  return results.length > 0 && results.every((result) => result.status === "passed")
    ? "pass"
    : "fail";
}

function reportTable(
  matrix: ApplicabilityMatrix,
  results: readonly ScenarioResult[],
  selector?: string,
): { text: string; failed: boolean } {
  const lines = ["tool | trajectory | applicable/n-a(reason) | pass/fail/expected_fail(issue)"];
  let failed = false;
  for (const row of matrix.rows.toSorted((left, right) => left.tool.localeCompare(right.tool))) {
    for (const trajectory of TRAJECTORIES) {
      const parent = `${row.tool}/${trajectory}`;
      if (
        selector &&
        parent !== selector.replace(/\/$/, "") &&
        !selector.startsWith(`${parent}/`)
      ) {
        continue;
      }
      const classification = row.trajectories[trajectory];
      const children = results.filter(
        (result) => result.id === parent || result.id.startsWith(`${parent}/`),
      );
      const disposition = parentDisposition(classification, children);
      if (disposition === "fail") failed = true;
      const applicability = classification.startsWith("n/a:") ? classification : "applicable";
      const renderedDisposition =
        disposition === "expected_fail"
          ? classification
          : disposition === "n/a"
            ? "n/a"
            : disposition;
      lines.push(`${row.tool} | ${trajectory} | ${applicability} | ${renderedDisposition}`);
      for (const child of children) {
        lines.push(
          `  ${child.id} | ${child.status}${child.failure ? ` | ${child.failure.code}` : ""}`,
        );
      }
    }
  }
  return { text: lines.join("\n"), failed };
}

async function main(): Promise<void> {
  const pinnedHostVersion = await readPinnedHostVersion(repoRoot);
  if (process.argv.includes("--print-host-version")) {
    console.log(pinnedHostVersion);
    return;
  }
  const config = await configuration();
  await mkdir(config.runRoot, { recursive: true });
  await verifyExecutableProvenance({
    executable: config.executable,
    repoRoot,
    policyPath: join(harnessRoot, "executable-policy.json"),
    manifestPath: join(config.runRoot, "provenance", "aft_binary.json"),
    checkoutSha: config.checkoutSha,
  });
  const controlEvidence = await runHarnessControlSuite(
    join(config.runRoot, "harness-control-fixtures"),
  );
  await assertHarnessControlCoverage(join(harnessRoot, "mutation-controls.json"), controlEvidence);
  await writeFile(
    join(config.runRoot, "harness-control-evidence.json"),
    `${JSON.stringify({ schema_version: 1, controls: controlEvidence }, null, 2)}\n`,
  );

  if (config.captureSchemaObservation) {
    const observed = await runOneScenario({
      scenario: HOST_SCHEMA_REJECTION_PROBE,
      config,
      pinnedHostVersion,
      pluginVersion: await readPackageVersion(),
      extensions: [],
      runSmoke: false,
    });
    if (observed.result.status === "failed") {
      throw new Error(
        `host schema-rejection observation failed: ${observed.result.failure?.message ?? "unknown"}`,
      );
    }
    console.log(
      `host schema-rejection observation captured at ${resolve(config.captureSchemaObservation)}; coverage not credited`,
    );
    return;
  }

  const scenarioRoot = join(repoRoot, "tests", "docker", "opencode2", "scenarios");
  const allScenarios = materializeParityScenarios(await loadScenarios(scenarioRoot));
  const scenarios = filterScenarios(allScenarios, config.selector);
  const validated = await validateHarnessInputs({
    repoRoot,
    scenarios: allScenarios,
    pinnedHostVersion,
    platform: "linux",
  });
  const extensions = await loadHarnessExtensions(join(repoRoot, "tests", "docker", "opencode2"));
  for (const extension of extensions) await extension.validate?.(validated.context);
  if (config.validateOnly) {
    console.log(`validated ${allScenarios.length} scenarios`);
    return;
  }

  const hostContract = await loadHostCliContract(
    join(repoRoot, "tests", "docker", "opencode2", "contract"),
    pinnedHostVersion,
  );
  const providerContract = await loadHostProviderConfigContract(
    join(repoRoot, "tests", "docker", "opencode2", "contract"),
    pinnedHostVersion,
  );
  const pluginVersion = await readPackageVersion();
  const results: ScenarioResult[] = [];
  let smokeRan = false;
  for (const scenario of scenarios) {
    let result: ScenarioResult;
    if (scenario.trajectory === "T7") {
      const v2 = await runOneScenario({
        scenario,
        config,
        pinnedHostVersion,
        pluginVersion,
        hostContract,
        extensions,
        runSmoke: false,
        hostGeneration: "v2",
        providerConfig: providerContract.provider_config,
        applyComparison: true,
      });
      const v1 = config.v1HostExecutable
        ? await runOneScenario({
            scenario,
            config,
            pinnedHostVersion,
            pluginVersion,
            extensions,
            runSmoke: false,
            hostGeneration: "v1",
            hostExecutable: config.v1HostExecutable,
            applyComparison: false,
          })
        : {
            result: {
              id: scenario.id,
              status: "failed" as const,
              failure: { code: "host_failed", message: "OPENCODE1_BIN is required for T7" },
              forensic_dir: dirname(v2.result.forensic_dir),
            },
            smokeRan: false,
            observedTexts: {},
          };
      result = v2.result.status === "failed" ? v2.result : v1.result;
      if (v2.result.status === "passed" && v1.result.status === "passed") {
        try {
          const callId = scenario.compare_call_id;
          if (!callId) throw new Error(`${scenario.id}: T7 requires compare_call_id`);
          assertDualHostParity(
            scenario,
            v1.observedTexts[callId] ?? "",
            v2.observedTexts[callId] ?? "",
            validated.parityAllowlist,
          );
          result = {
            id: scenario.id,
            status: "passed",
            issue: scenario.expected_fail_issue,
            forensic_dir: dirname(v2.result.forensic_dir),
          };
        } catch (error) {
          const parityForensics = new ScenarioForensics(config.runRoot, scenario.id);
          await parityForensics.initialize(scenario);
          await parityForensics.recordFailure(error);
          result = {
            id: scenario.id,
            status: "failed",
            issue: scenario.expected_fail_issue,
            failure: errorRecord(error),
            forensic_dir: parityForensics.directory,
          };
        }
      }
    } else {
      const outcome = await runOneScenario({
        scenario,
        config,
        pinnedHostVersion,
        pluginVersion,
        hostContract,
        extensions,
        providerConfig: providerContract.provider_config,
        runSmoke: !smokeRan && scenario.execution === "shared-server",
      });
      result = outcome.result;
      smokeRan ||= outcome.smokeRan;
    }
    results.push(result);
    if (result.status === "failed") {
      console.error(`FAIL ${scenario.id}: ${result.failure?.message}`);
      console.error(`forensics: ${result.forensic_dir}`);
    }
  }
  if (!smokeRan && scenarios.some((scenario) => scenario.execution === "shared-server")) {
    throw new Error("shared-server smoke did not run");
  }
  const report = reportTable(validated.matrix, results, config.selector);
  console.log(report.text);
  console.log(`required check: ${validated.requiredCheckStatus}`);
  await writeFile(
    join(config.runRoot, "report.json"),
    `${JSON.stringify({ pinned_host_version: pinnedHostVersion, results }, null, 2)}\n`,
  );
  if (report.failed) process.exitCode = 1;
}

main().catch((error) => {
  console.error(error instanceof Error ? (error.stack ?? error.message) : String(error));
  process.exitCode = 1;
});
