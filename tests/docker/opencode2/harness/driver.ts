#!/usr/bin/env bun
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { parseE2EConcurrency, mapWithConcurrency } from "./concurrency.js";
import {
  loadHostCliContract,
  loadHostProviderConfigContract,
  loadV1HostProviderConfigContract,
} from "./contracts.js";
import { assertHarnessControlCoverage, runHarnessControlSuite } from "./control-suite.js";
import { DiskStateObserver, ThreeStateRecorder } from "./disk-state.js";
import { fail, HarnessError } from "./errors.js";
import { HostEventRecorder } from "./event-stream.js";
import { loadHarnessExtensions } from "./extensions.js";
import { ScenarioForensics, parseArtifactRetention, pruneOldRunRoots } from "./forensics.js";
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
import { assertT5HostWakeTranscript } from "./liveness.js";
import {
  DeterministicScenarioMock,
  materializeTurnPlaceholders,
  toolCallsInTurn,
  toolResultForCall,
} from "./mock-server.js";
import {
  assertBashAftExecutionIdentity,
  assertBashDeadTransportRefusal,
  assertBashExecutionPathIdentity,
  assertConfigDenyHidesTool,
  assertPermissionPromptObserved,
  controlPlans,
  sessionPermissionRules,
} from "./permission-plan.js";
import { readPinnedHostVersion, readPinnedV1HostVersion } from "./pin.js";
import {
  type AbortSettlementEvidence,
  callAbortedFailure,
  INTERRUPTION_BUDGET_MS,
  ProcessObserver,
  waitForAbortSettlement,
  waitForTaskStatus,
} from "./process-observer.js";
import { reportTable } from "./report.js";
import { AftTaskProbe } from "./task-probe.js";
import { assertComparison, assertT6Trailer, projectText } from "./projection.js";
import { verifyExecutableProvenance } from "./provenance.js";
import { filterScenarios, loadScenarios, materializeParityScenarios } from "./scenario-loader.js";
import {
  HOST_SCHEMA_REJECTION_PROBE,
  writeHostSchemaRejectionObservation,
} from "./schema-observation.js";
import { assertTurnLog, readTurnLog } from "./turn-log.js";
import {
  makeTransportDeadStub,
  pointTransportDeadStub,
  type TransportDeadStub,
} from "./transport-stub.js";
import { resolveTransportDeadWindow, transportDeadAtTurn } from "./transport-window.js";
import type {
  ApiControlPlan,
  HarnessRuntimeEvent,
  RecordedMockExchange,
  ScenarioDefinition,
  ScenarioLifecycleContext,
  ScenarioResult,
  SessionPermissionRule,
  ToolCallPlan,
} from "./types.js";
import {
  type ParityAllowlistEntry,
  type ValidatedInputs,
  type VerdictExclusion,
  validateHarnessInputs,
  verdictExclusionFor,
} from "./validation.js";
import { asRecord, createRunId, runCommand } from "./util.js";

const harnessRoot = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(harnessRoot, "../../../..");

/**
 * How long a host whose exit is excluded is given to end itself once it has
 * served the whole scenario.
 *
 * Long enough that a host which does terminate is recorded as having
 * terminated, rather than killed by a harness in a hurry; short enough that a
 * host which never terminates costs the row seconds instead of its whole
 * timeout.
 */
const SERVED_HOST_SETTLE_MS = 5_000;

interface DriverConfig {
  executable: string;
  nativeExecutable: string;
  hostExecutable: string;
  v1HostExecutable?: string;
  pluginTarball: string;
  pluginDirectory: string;
  runRoot: string;
  /** How many run roots the artifact store keeps, including this run's own. */
  artifactRetention: number;
  selector?: string;
  concurrency: number;
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
    nativeExecutable: resolve(requiredEnvironment("AFT_E2E_NATIVE_BINARY_PATH")),
    hostExecutable: resolve(requiredEnvironment("OPENCODE2_BIN")),
    v1HostExecutable: process.env.OPENCODE1_BIN ? resolve(process.env.OPENCODE1_BIN) : undefined,
    pluginTarball: resolve(requiredEnvironment("AFT_OPENCODE2_PLUGIN_TARBALL")),
    pluginDirectory: resolve(requiredEnvironment("AFT_OPENCODE2_PLUGIN_DIRECTORY")),
    runRoot,
    artifactRetention: parseArtifactRetention(process.env.AFT_E2E_ARTIFACT_RETAIN),
    selector: process.env.AFT_E2E_SCENARIO ?? argumentValue("--scenario"),
    concurrency: parseE2EConcurrency(process.env.AFT_E2E_CONCURRENCY),
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

/** Everything the scenario's scripted calls handed back to the model. */
function scriptedResultText(
  scenario: ScenarioDefinition,
  exchanges: readonly RecordedMockExchange[],
): string {
  return plannedCalls(scenario)
    .map((call) => toolResultForCall(exchanges, call.id)?.text ?? "")
    .join("\n");
}

/**
 * Put a scenario's permission rules on the host session before its first call.
 *
 * Installing them from the model's first request is what makes this race-free:
 * the host has created the session by then and cannot start a tool call before
 * the response that carries it, so the rules are in place by the time anything
 * is gated.
 */
async function installSessionPermissionRules(
  scenario: ScenarioDefinition,
  rules: readonly SessionPermissionRule[],
  sessionId: string,
  options: Omit<Parameters<typeof runApiCommand>[0], "method" | "path" | "body">,
): Promise<void> {
  const applied = await runApiCommand({
    ...options,
    method: "PATCH",
    path: `/api/session/${encodeURIComponent(sessionId)}`,
    body: { permissions: rules },
    timeoutMs: 10_000,
  });
  if (applied.exit_code !== 0) {
    fail("host_failed", `${scenario.id}: installing session permission rules failed`, {
      rules,
      session_id: sessionId,
      output: applied,
    });
  }
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

async function discoverActiveSessionId(
  control: ApiControlPlan,
  values: Record<string, string>,
  options: Omit<Parameters<typeof runApiCommand>[0], "method" | "path" | "body">,
): Promise<void> {
  if (!control.path.includes("{{session_id}}") || values.session_id) return;
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    try {
      const authorization = Buffer.from(`opencode:${options.password}`).toString("base64");
      const response = await fetch(new URL("/api/session/active", options.endpoint), {
        headers: { authorization: `Basic ${authorization}` },
      });
      if (response.ok) {
        const data = asRecord(await response.json())?.data;
        if (typeof data === "object" && data !== null && !Array.isArray(data)) {
          const activeId = Object.keys(data)[0];
          if (activeId) {
            values.session_id = activeId;
            return;
          }
        }
      }
    } catch {}
    await Bun.sleep(25);
  }
  fail("host_failed", `${control.id}: active session id was not observed`);
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

/**
 * Refuse a run whose scripted tool was never registered.
 *
 * The host answers a call for an unknown tool with "No tool named ...", which
 * looks like a clean run to every assertion a scenario makes about not
 * mutating anything: nothing ran, so nothing changed. A row that passes that
 * way has tested the host's error message, not the product, so the absence is
 * reported by name instead.
 */
function assertScriptedToolsRegistered(scenario: ScenarioDefinition, hostStream: string): void {
  const missing = [
    ...new Set(
      [...hostStream.matchAll(/No tool named \\?"([^"\\]+)\\?" is currently available/g)].map(
        (match) => match[1],
      ),
    ),
  ];
  if (missing.length > 0) {
    fail("host_failed", `${scenario.id}: the host registered none of ${missing.join(", ")}`, {
      missing_tools: missing,
    });
  }
}

/**
 * Require an interrupted run to say it was interrupted, in its own words.
 *
 * A one-shot run that is cut off mid-tool does not exit 0, and the exit code
 * alone cannot tell an interruption apart from a host that fell over: both are
 * a non-zero number. The host does say which happened — it writes an error
 * part naming the abort onto its event stream — so a row that accepts the
 * wider exit set names the error it expects and is held to finding it.
 */
function assertAbortEnding(scenario: ScenarioDefinition, hostStream: string): void {
  const expected = asRecord(scenario.metadata?.t4)?.expected_error_type;
  if (typeof expected !== "string") return;
  const observed = hostStream.split(/\r?\n/).some((line) => {
    if (!line.trim()) return false;
    try {
      const event = asRecord(JSON.parse(line));
      return event?.type === "error" && asRecord(event.error)?.type === expected;
    } catch {
      return false;
    }
  });
  if (!observed) {
    throw new Error(
      `${scenario.id}: the host's event stream carries no ${expected} error, so the run's ending is not the interruption the row claims`,
    );
  }
}

function errorRecord(error: unknown): ScenarioResult["failure"] {
  if (error instanceof HarnessError) {
    return {
      code: error.code,
      message: error.message,
      details: { ...error.details },
      unsuppressible: error.unsuppressible,
    };
  }
  return {
    code: error instanceof Error ? error.name : "NonError",
    message: error instanceof Error ? error.message : String(error),
  };
}

async function killAftBridgeDescendants(
  serverPid: number | undefined,
  configuredExecutables: readonly string[],
): Promise<void> {
  if (!serverPid) return;
  const processList = await runCommand("ps", ["-eo", "pid=,ppid=,command="], {
    cwd: repoRoot,
    timeoutMs: 5_000,
  });
  if (processList.exit_code !== 0) {
    fail("host_failed", "could not inspect the shared-server process tree", {
      output: processList,
    });
  }
  const rows = processList.stdout
    .split("\n")
    .map((line) => line.match(/^\s*(\d+)\s+(\d+)\s+(.+)$/))
    .filter((match): match is RegExpMatchArray => match !== null)
    .map((match) => ({ pid: Number(match[1]), parent: Number(match[2]), command: match[3] }));
  const descendants = new Set([serverPid]);
  let added = true;
  while (added) {
    added = false;
    for (const row of rows) {
      if (!descendants.has(row.parent) || descendants.has(row.pid)) continue;
      descendants.add(row.pid);
      added = true;
    }
  }
  for (const row of rows) {
    if (
      !descendants.has(row.pid) ||
      !configuredExecutables.some((candidate) => row.command.includes(candidate))
    ) {
      continue;
    }
    try {
      process.kill(row.pid, "SIGKILL");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
    }
  }
  await Bun.sleep(50);
}

async function readPackageVersion(): Promise<string> {
  const manifest = JSON.parse(
    await readFile(join(repoRoot, "packages", "opencode-plugin", "package.json"), "utf8"),
  ) as { version?: string };
  if (!manifest.version) throw new Error("opencode plugin package version is missing");
  return manifest.version;
}

/**
 * The part of a host's own output that says why it stopped.
 *
 * "host exited 1" alone sends the next reader into the forensics tree to find
 * out what happened, and a host that dies during startup has usually already
 * written the reason. Level-tagged lines come first because that is where both
 * host generations put their diagnostics; the plain tail is the fallback for
 * output that carries no levels.
 */
function hostFailureMechanism(output: { stdout: string; stderr: string }): string {
  const lines = `${output.stderr}\n${output.stdout}`
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  const diagnostics = lines.filter((line) => /level=(ERROR|WARN)|"type":"error"/.test(line));
  return (diagnostics.length > 0 ? diagnostics : lines)
    .slice(-3)
    .map((line) => (line.length > 200 ? `${line.slice(0, 200)}…` : line))
    .join(" | ");
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
  /**
   * Set only for a leg whose row declares that this host's process exit is not
   * part of its verdict. Absent, an unaccepted exit fails the leg as usual.
   */
  excludedHostExit?: VerdictExclusion;
  providerConfig: Record<string, unknown>;
  providerConfigKey: string;
  providerModel: string;
}): Promise<{
  result: ScenarioResult;
  smokeRan: boolean;
  observedTexts: Record<string, string>;
}> {
  const { scenario, config } = options;
  const startedAt = Date.now();
  const hostGeneration = options.hostGeneration ?? "v2";
  const hostExecutable = options.hostExecutable ?? config.hostExecutable;
  const forensicId =
    scenario.trajectory === "T7" ? `${scenario.id}/${hostGeneration}` : scenario.id;
  const forensics = new ScenarioForensics(config.runRoot, forensicId);
  await forensics.initialize(scenario);
  const turnLogPath = join(forensics.directory, "turns.log");
  let disk: DiskStateObserver | undefined;
  let processObserver: ProcessObserver | undefined;
  let taskProbe: AftTaskProbe | undefined;
  let threeState: ThreeStateRecorder | undefined;
  let server: SharedServerHandle | undefined;
  let hostEvents: HostEventRecorder | undefined;
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
  // The task an abort control was gated on, so teardown can let the product
  // end that task before the harness stops anything.
  let abortTargetTaskId: string | undefined;
  let hostCompletedAt: number | undefined;
  const controlPathValues: Record<string, string> = {};
  const permissionRules = sessionPermissionRules(scenario);
  const rejectedPermission = asRecord(scenario.metadata?.permission)?.reply === "reject";
  let permissionRulesInstalled = false;
  const transportDeadWindow = resolveTransportDeadWindow(scenario);
  let transportDeadStub: TransportDeadStub | undefined;
  let transportDeadActive = false;
  let excludedHostExit: Record<string, unknown> | undefined;

  /**
   * Resolves once the mock has answered the scenario's last scripted turn.
   *
   * That is the moment the host has been given everything the row asks of it:
   * every turn has been delivered, and each tool result came back inside the
   * request for the turn after it. Nothing the harness still has to read comes
   * off this process afterwards.
   */
  let markServed: () => void = () => {};
  const scenarioServed = new Promise<void>((resolvePromise) => {
    markServed = resolvePromise;
  });

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

  /**
   * Take the three-state snapshots that are keyed to a call's result.
   *
   * Only the first and last of them are: the state before the sequence starts,
   * read once the opening call has reported, and the restored state, read once
   * the restoring call has. The middle state is taken when the restoring call
   * BEGINS instead — see the capture in the turn hook for why the mutating
   * call's own result is too late a moment to use.
   */
  const captureRestoreResult = async (call: ToolCallPlan) => {
    if (!threeState) return;
    const index = restoreCalls.findIndex((candidate) => candidate.id === call.id);
    if (index === -1) return;
    if (index === restoreCalls.length - 1) await threeState.capture("restored");
    else if (index === 0 && restoreCalls.length > 2) await threeState.capture("before");
  };

  try {
    mock = new DeterministicScenarioMock(scenario, turnLogPath, {
      beforeTurn: async (turn, request) => {
        // The host has created the session by the time it asks for a response
        // and cannot run a tool before receiving one, so this is the last
        // moment that is both late enough to name the session and early enough
        // to precede every gated call.
        if (
          permissionRules &&
          !permissionRulesInstalled &&
          server &&
          isolation &&
          hostEvents &&
          options.hostContract
        ) {
          permissionRulesInstalled = true;
          try {
            const sessionId = await hostEvents.awaitSessionId(20_000);
            if (!sessionId) {
              fail("host_failed", `${scenario.id}: the host announced no session to gate`, {
                event_stream_failure: hostEvents.failure,
              });
            }
            controlPathValues.session_id = sessionId;
            await installSessionPermissionRules(scenario, permissionRules, sessionId, {
              executable: hostExecutable,
              cwd: isolation.project,
              env: isolation.env,
              contract: options.hostContract,
              endpoint: server.endpoint,
              password: server.password,
            });
            await forensics.writeJson("session-permission-rules.json", {
              session_id: sessionId,
              rules: permissionRules,
            });
          } catch (error) {
            recordFailure(error);
          }
        }
        if (transportDeadWindow && transportDeadStub) {
          const shouldBeDead = transportDeadAtTurn(scenario, transportDeadWindow, turn.label);
          if (shouldBeDead !== transportDeadActive) {
            await pointTransportDeadStub(transportDeadStub, shouldBeDead);
            if (shouldBeDead) {
              await killAftBridgeDescendants(server?.child.pid, [
                transportDeadStub.executable,
                config.nativeExecutable,
              ]);
            }
            transportDeadActive = shouldBeDead;
          }
        }
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
            // The mutated state, read at the last moment before the restoring
            // call is issued. Reading it when the mutating call's RESULT is
            // observed instead loses rows whose host hands that result back
            // late: the host can start the next tool call, and answer the
            // model's next request, while a tool part still carries no output,
            // and by the time the result turns up the restore has already run
            // — which reads as a mutation that never happened. A capture taken
            // too early is still caught, because a mutation that has not
            // landed leaves this state equal to the one before it.
            if (threeState && call.id === restoreCalls.at(-1)?.id && !threeState.intermediate) {
              await threeState.capture("intermediate");
            }
            await disk.beginCall(call);
            begun.add(call.id);
          } catch (error) {
            recordFailure(error);
          }
        }
      },
      afterRequest: async (exchange, turn) => {
        await emit({ kind: "mock_exchange", exchange });
        if (turn) materializeTurnPlaceholders(turn, controlPathValues);
      },
      afterResponse: async (exchange) => {
        if (exchange.index === scenario.turns.length - 1) markServed();
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
            if (control.wait_for_task) {
              if (!taskProbe) throw new Error(`${scenario.id}: task probe is unavailable`);
              const readiness = await waitForTaskStatus(
                taskProbe,
                control.wait_for_task.status,
                control.wait_for_task.timeout_ms,
              );
              controlPathValues.task_id = readiness.task.id;
              if (control.purpose === "abort") abortTargetTaskId = readiness.task.id;
              await forensics.writeJson(`control-${control.id}-task-readiness.json`, readiness);
            }
            await discoverActiveSessionId(control, controlPathValues, apiOptions);
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
      pluginDirectory: config.pluginDirectory,
      pluginVersion: options.pluginVersion,
      hostGeneration,
      binaryPath: config.nativeExecutable,
      mockBaseUrl: mock.url,
      projectConfig: scenario.project_config,
      providerConfig: options.providerConfig,
      providerConfigKey: options.providerConfigKey,
    });
    if (transportDeadWindow) {
      transportDeadStub = await makeTransportDeadStub(isolation.root, config.nativeExecutable);
      isolation.env.AFT_BINARY_PATH = transportDeadStub.executable;
    } else {
      isolation.env.AFT_BINARY_PATH = config.nativeExecutable;
    }
    disk = new DiskStateObserver(isolation.project, scenario.id);
    restoreCalls = restoreCallSequence(scenario);
    if (scenario.restore_evidence) {
      threeState = new ThreeStateRecorder(isolation.project, scenario.restore_evidence.paths);
    }
    taskProbe = new AftTaskProbe(join(isolation.data, "cortexkit", "aft", "aft.db"));
    processObserver = new ProcessObserver(scenario.id, taskProbe);
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
        stateRoot: isolation.state,
      });
      await forensics.writeJson("service-registration.json", server.registration);
      // Opened before the client starts so a request that is created and
      // answered in the same instant is still on record.
      hostEvents = new HostEventRecorder();
      await hostEvents.start(server.endpoint, server.password);
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
      model: options.providerModel,
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
    const hostTimeoutMs =
      typeof scenario.metadata?.host_timeout_ms === "number"
        ? scenario.metadata.host_timeout_ms
        : 45_000;
    // A leg whose row has already taken this host's exit out of its verdict is
    // not waited on to the row timeout. Once the scenario has been served the
    // harness has everything it judges, so it allows a short settling window —
    // enough for a host that does mean to exit to do so, and to be recorded as
    // having exited — and then stops waiting. The pinned V1 host does not exit
    // inside a git repository, and paying its full row timeout on every parity
    // row was most of what the matrix spent. A leg that never serves resolves
    // nothing here and still runs out its own timeout.
    const stopWaiting = options.excludedHostExit
      ? scenarioServed.then(() => Bun.sleep(SERVED_HOST_SETTLE_MS))
      : undefined;
    const host = await client.wait(hostTimeoutMs, stopWaiting);
    hostCompletedAt = Date.now();
    await emit({ kind: "host_exit", at: hostCompletedAt, output: host });
    hostStream = host.stdout;
    await forensics.writeJson("host-command.json", host);
    await forensics.writeText("host-stream.ndjson", host.stdout);
    await forensics.writeText("host-stderr.log", host.stderr);
    const acceptedExitCodes = Array.isArray(scenario.metadata?.accepted_exit_codes)
      ? scenario.metadata.accepted_exit_codes
      : [0];
    const servedTurns = mock.exchanges.length;
    const scriptedTurns = scenario.turns.length;
    const pace = `served ${servedTurns} of ${scriptedTurns} scripted turns; the model received ${mock.requests.length} request(s)`;
    if (!acceptedExitCodes.includes(host.exit_code)) {
      const mechanism = hostFailureMechanism(host);
      // A host that ran out of time without answering a single scripted turn
      // did not fail at what the row measures: nothing the row measures had
      // been asked for yet. Saying so by name is what lets a reader tell a
      // start that never got going from a failure in the tool under test,
      // instead of reading the same "still running" line for both. The name
      // says where the run stopped, not whose fault it was — a plugin that
      // hangs while registering stalls a start too — and the counts beside it
      // are what a reader checks that against.
      if (host.timed_out && servedTurns === 0) {
        fail(
          "host_startup_stall",
          `${scenario.id}: the host ran out its ${hostTimeoutMs}ms budget having ${pace}${
            mechanism ? `; host said: ${mechanism}` : ""
          }`,
          {
            host_generation: hostGeneration,
            mock_requests: mock.requests.length,
            scripted_turns: scriptedTurns,
            served_turns: servedTurns,
            timeout_ms: hostTimeoutMs,
          },
        );
      }
      // A killed host reports no exit code, and "exited null" reads like a
      // crash; saying it ran out of time is the difference between looking for
      // a fatal error and looking for what it was still waiting on.
      const ending = host.stopped_early
        ? `host was still running ${SERVED_HOST_SETTLE_MS}ms after it had served the whole scenario, and was stopped`
        : host.timed_out
          ? `host was still running after ${hostTimeoutMs}ms and was killed`
          : `host exited ${host.exit_code}`;
      // An exclusion covers the ending of a host that has already served the
      // whole scenario. One that was still mid-scenario when its time ran out
      // has not earned it: that ending is the failure, and reporting it here
      // keeps it from surfacing further down as a missing tool result, which
      // describes the consequence rather than what happened.
      if (options.excludedHostExit && servedTurns >= scriptedTurns) {
        // The row says this host's exit is not part of its verdict, so the
        // ending is recorded and the run carries on to the checks that are.
        // Nothing else is relaxed: the assertions below still require this
        // host to have served the whole scenario.
        excludedHostExit = {
          ...options.excludedHostExit,
          host_generation: hostGeneration,
          observed_ending: `${ending}${mechanism ? `; host said: ${mechanism}` : ""}`,
          exit_code: host.exit_code,
          signal: host.signal,
          timed_out: host.timed_out,
          stopped_after_serving: host.stopped_early === true,
          settle_ms: SERVED_HOST_SETTLE_MS,
        };
        await forensics.writeJson("excluded-host-exit.json", excludedHostExit);
      } else {
        throw new Error(
          `${ending}; ${pace}; accepted ${acceptedExitCodes.join(",")}${
            mechanism ? `; host said: ${mechanism}` : ""
          }`,
        );
      }
    }
    assertScriptedToolsRegistered(scenario, hostStream);
    assertAbortEnding(scenario, hostStream);
    // Scoped to the rows whose host behaviour was observed. The check itself
    // describes any wholly-denying rule and can be widened once another tool's
    // rows have been watched doing the same thing.
    if (scenario.id.startsWith("bash/T3/")) assertConfigDenyHidesTool(scenario, mock.exchanges);
    // Ahead of the generic permission assertion: when a bash row's declared
    // execution path (the AFT loop, or the break-glass fallback) is the one
    // that raises its own ask, "which ask is missing" is the more specific
    // account of the same absence. A row whose dead transport is declared to
    // produce a refusal instead has no ask to miss, and the second assertion
    // is what reads that refusal back out of the model's own view of the call.
    const bashExecutionPath = {
      resultText: scriptedResultText(scenario, mock.exchanges),
      events: hostEvents?.events ?? [],
    };
    assertBashExecutionPathIdentity(scenario, bashExecutionPath);
    assertBashDeadTransportRefusal(scenario, bashExecutionPath);
    if (hostEvents) assertPermissionPromptObserved(scenario, hostEvents.events);
    await Promise.all(controlPromises);
    if (
      scenario.trajectory === "T4" &&
      (abortIssuedAt === undefined || hostCompletedAt - abortIssuedAt > INTERRUPTION_BUDGET_MS)
    ) {
      throw new Error(
        `${scenario.id}: interruption exceeded ${INTERRUPTION_BUDGET_MS / 1_000}s (${String(
          abortIssuedAt === undefined ? "abort not issued" : hostCompletedAt - abortIssuedAt,
        )})`,
      );
    }
    if (firstFailure) throw firstFailure;

    for (const call of plannedCalls(scenario)) {
      const observed = toolResultForCall(mock.exchanges, call.id);
      if (!observed && scenario.trajectory === "T4") continue;
      // A rejected permission ends the run at the gated call: the host abandons
      // the session instead of feeding the tool result back to the model, so
      // there is no later request carrying it. What the row asserts instead is
      // the permission event pair and the absence of a disk effect.
      if (!observed && rejectedPermission) continue;
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
    const expectedTurns =
      scenario.trajectory === "T4"
        ? [scenario.turns[0].label]
        : (scenario.expected_turns ?? scenario.turns.map((turn) => turn.label));
    assertTurnLog(expectedTurns, await readTurnLog(turnLogPath));
    assertT5HostWakeTranscript(scenario, mock.exchanges, controlPathValues);

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
    if (transportDeadStub) await pointTransportDeadStub(transportDeadStub, false);
    await Promise.allSettled(controlPromises);
    // The host's client exits as soon as the interrupt is answered, while the
    // plugin's abort of the foreground task is still on its way to Rust. The
    // task and the host stay up until that abort has had the interruption
    // budget to land; stopping them any earlier is the harness killing the task
    // and then reading its own kill back as the product's.
    let abortSettlement: AbortSettlementEvidence | undefined;
    if (
      scenario.trajectory === "T4" &&
      taskProbe &&
      abortTargetTaskId !== undefined &&
      abortIssuedAt !== undefined
    ) {
      try {
        abortSettlement = await waitForAbortSettlement(
          taskProbe,
          abortTargetTaskId,
          abortIssuedAt + INTERRUPTION_BUDGET_MS,
        );
        await forensics.writeJson("abort-settlement.json", abortSettlement);
      } catch (error) {
        recordFailure(error);
      }
    }
    if (mock) {
      try {
        await forensics.writeExchanges(mock.exchanges);
        await forensics.writeJson("mock-requests.json", mock.requests);
      } catch (error) {
        recordFailure(error);
      }
    }
    if (isolation) {
      try {
        pluginLog = await readFile(isolation.plugin_log, "utf8");
        await forensics.writeText("plugin.log", pluginLog);
      } catch (error) {
        recordFailure(error);
      }
    }
    if (server) {
      try {
        await forensics.writeText("shared-server-stdout.log", server.stdout());
        await forensics.writeText("shared-server-stderr.log", server.stderr());
      } catch (error) {
        recordFailure(error);
      }
    }
    if (hostEvents) {
      try {
        await hostEvents.stop();
        await forensics.writeJson("host-events.json", {
          failure: hostEvents.failure,
          permission_asked: hostEvents.ofType("permission.asked"),
          permission_replied: hostEvents.ofType("permission.replied"),
          types: [...new Set(hostEvents.events.map((event) => event.type))].sort(),
        });
      } catch (error) {
        recordFailure(error);
      }
    }
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
      if (scenario.id.startsWith("bash/T3/")) {
        try {
          assertBashAftExecutionIdentity(
            scenario,
            termination.tasks.map((task) => task.id),
          );
        } catch (error) {
          recordFailure(error);
        }
      }
      if (scenario.trajectory === "T4") {
        const failure = callAbortedFailure(scenario.id, termination, abortSettlement);
        if (failure) recordFailure(failure);
      }
      if (disk) {
        // A rejected permission ends the run at the gated call, so no later
        // model request carries its result and the usual result-phase
        // checkpoint never happens. The host has exited by now, so the tree is
        // final and this checkpoint is what proves the refused call changed
        // nothing.
        if (rejectedPermission) {
          for (const call of plannedCalls(scenario).filter(
            (candidate) => begun.has(candidate.id) && !resultObserved.has(candidate.id),
          )) {
            try {
              await disk.checkpointCall(call.id, "result-after-rejected-permission", "result");
              resultObserved.add(call.id);
            } catch (error) {
              recordFailure(error);
            }
          }
        }
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

  const applied = excludedHostExit ? [String(excludedHostExit.subject)] : undefined;
  let result: ScenarioResult = firstFailure
    ? {
        id: scenario.id,
        status: "failed",
        issue: scenario.expected_fail_issue,
        exclusions: applied,
        failure: errorRecord(firstFailure),
        forensic_dir: forensics.directory,
      }
    : {
        id: scenario.id,
        status: "passed",
        issue: scenario.expected_fail_issue,
        exclusions: applied,
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
          exclusions: applied,
          failure: errorRecord(error),
          forensic_dir: forensics.directory,
        };
        await forensics.recordFailure(error);
      }
    }
  }
  result.elapsed_ms = Date.now() - startedAt;
  return { result, smokeRan, observedTexts };
}

/**
 * The V1 host's process exit, when the row has taken it out of its verdict.
 *
 * T7 asks one question: do the two host generations produce the same tool
 * behaviour? The served scenario answers it in full — the turns are delivered,
 * the tool runs, and its result comes back to be compared. The pinned
 * OpenCode 1 build then never terminates inside a git repository, which every
 * scenario project is, so the harness kills it at the row's timeout. That
 * ending is an upstream teardown defect, and judging parity by it means the
 * row reports on something it does not exist to measure.
 *
 * So a row may declare that this leg's exit is not its business, and the
 * harness leaves out the exit status and its own kill of a host that has
 * already served — nothing else. Both turns still have to be delivered, the
 * tool result still has to arrive, and it still has to match the other host's,
 * or the row fails. A leg that never served fails on those checks, which is
 * what keeps "served but did not exit" apart from "did not serve".
 */
function v1ExitExclusion(
  matrix: ValidatedInputs["matrix"],
  scenario: ScenarioDefinition,
): VerdictExclusion | undefined {
  return declaredExclusion(matrix, scenario, "v1_host_process_exit");
}

/**
 * The V1 leg itself, when the host cannot be asked the row's question.
 *
 * The pinned OpenCode 1 build chooses between `apply_patch` and `edit`/`write`
 * from the model id, inside its own tool registry and before any plugin is
 * consulted: a GPT-5-shaped id gets apply_patch and loses edit and write, and
 * every other id gets edit and write and loses apply_patch. The harness drives
 * a mock model, so the V1 leg of an apply_patch row is offered a catalogue
 * that cannot contain the tool under test, and the host refuses the scripted
 * call before the plugin is reached. Renaming the mock to earn apply_patch
 * would flip the same switch the other way and take edit and write off the V1
 * host, so the row would be bought with two others.
 *
 * A row may therefore declare that its V1 leg is not its business, and the
 * harness runs the V2 leg alone and leaves the cross-host comparison out. The
 * V2 leg is unchanged: the tool still has to be registered, called, and its
 * result still has to match what the scenario declares, or the row fails. The
 * gate the exclusion rests on is read back out of the pinned host's own bundle
 * on every run (`contract/host1-edit-family-gate.json`).
 */
function v1EditFamilyExclusion(
  matrix: ValidatedInputs["matrix"],
  scenario: ScenarioDefinition,
): VerdictExclusion | undefined {
  return declaredExclusion(matrix, scenario, "v1_host_edit_family_gate");
}

function declaredExclusion(
  matrix: ValidatedInputs["matrix"],
  scenario: ScenarioDefinition,
  subject: string,
): VerdictExclusion | undefined {
  if (!matrix) return undefined;
  const declared = verdictExclusionFor(matrix, scenario.tool, scenario.trajectory);
  return declared?.subject === subject ? declared : undefined;
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

async function main(): Promise<void> {
  const pinnedHostVersion = await readPinnedHostVersion(repoRoot);
  if (process.argv.includes("--print-host-version")) {
    console.log(pinnedHostVersion);
    return;
  }
  const config = await configuration();
  await mkdir(config.runRoot, { recursive: true });
  // Forensics are worth keeping for the run being read and the couple before
  // it. Older run roots are scenario trees nobody opens again, and nothing
  // else removes them, so the artifact store only grows.
  const removedRunRoots = await pruneOldRunRoots({
    parent: dirname(config.runRoot),
    keep: config.artifactRetention,
    current: config.runRoot,
  });
  if (removedRunRoots.length > 0) {
    console.log(
      `removed ${removedRunRoots.length} run root(s) beyond the newest ${config.artifactRetention}: ${removedRunRoots
        .map((path) => basename(path))
        .join(", ")}`,
    );
  }
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

  const contractRoot = join(repoRoot, "tests", "docker", "opencode2", "contract");
  const providerContract = await loadHostProviderConfigContract(contractRoot, pinnedHostVersion);

  if (config.captureSchemaObservation) {
    const observed = await runOneScenario({
      scenario: HOST_SCHEMA_REJECTION_PROBE,
      config,
      pinnedHostVersion,
      pluginVersion: await readPackageVersion(),
      extensions: [],
      runSmoke: false,
      providerConfig: providerContract.provider_config,
      providerConfigKey: providerContract.config_key,
      providerModel: providerContract.model,
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
  console.log(`running ${scenarios.length} scenarios with concurrency ${config.concurrency}`);
  const validated = await validateHarnessInputs({
    repoRoot,
    scenarios: allScenarios,
    pinnedHostVersion,
    platform: "linux",
    fullRun: config.selector === undefined,
    v1HostExecutable: config.v1HostExecutable,
  });
  const extensions = await loadHarnessExtensions(join(repoRoot, "tests", "docker", "opencode2"));
  for (const extension of extensions) await extension.validate?.(validated.context);
  if (config.validateOnly) {
    console.log(`validated ${allScenarios.length} scenarios`);
    return;
  }

  const hostContract = await loadHostCliContract(contractRoot, pinnedHostVersion);
  // T7 runs each row on both hosts, and the V1 leg gets its own captured
  // provider observation. The two hosts take different provider shapes under
  // different keys, and handing V1 the V2 one leaves it with no provider.
  const v1ProviderContract = scenarios.some((scenario) => scenario.trajectory === "T7")
    ? await loadV1HostProviderConfigContract(
        contractRoot,
        await readPinnedV1HostVersion(repoRoot),
      )
    : undefined;
  const pluginVersion = await readPackageVersion();
  const smokeScenarioId = scenarios.find(
    (scenario) => scenario.execution === "shared-server" && scenario.trajectory !== "T7",
  )?.id;
  const outcomes = await mapWithConcurrency(scenarios, config.concurrency, async (scenario) => {
    const startedAt = Date.now();
    let result: ScenarioResult;
    let smokeRan = false;
    if (scenario.trajectory === "T7") {
      // Each leg gets its own copy of the script. A turn's placeholders are
      // filled in on the turn itself as the run observes the ids it names, so
      // two legs sharing one scenario would leave the second calling the
      // first's background task instead of its own.
      const v2 = await runOneScenario({
        scenario: structuredClone(scenario),
        config,
        pinnedHostVersion,
        pluginVersion,
        hostContract,
        extensions,
        runSmoke: false,
        hostGeneration: "v2",
        providerConfig: providerContract.provider_config,
        providerConfigKey: providerContract.config_key,
        providerModel: providerContract.model,
        applyComparison: true,
      });
      const editFamilyExclusion = v1EditFamilyExclusion(validated.matrix, scenario);
      if (editFamilyExclusion) {
        // The V1 host withholds this row's tool from the model the harness
        // drives, so there is no second leg to compare. What the V2 leg proved
        // still stands, and the report says which subject was left out.
        result = {
          ...v2.result,
          exclusions: [editFamilyExclusion.subject],
          ...(v2.result.status === "passed"
            ? { forensic_dir: dirname(v2.result.forensic_dir) }
            : {}),
        };
        result.elapsed_ms = Date.now() - startedAt;
        return { result, smokeRan };
      }
      const v1 =
        config.v1HostExecutable && v1ProviderContract
          ? await runOneScenario({
              scenario: structuredClone(scenario),
              config,
              pinnedHostVersion,
              pluginVersion,
              extensions,
              runSmoke: false,
              hostGeneration: "v1",
              hostExecutable: config.v1HostExecutable,
              providerConfig: v1ProviderContract.provider_config,
              providerConfigKey: v1ProviderContract.config_key,
              providerModel: v1ProviderContract.model,
              applyComparison: false,
              excludedHostExit: v1ExitExclusion(validated.matrix, scenario),
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
            exclusions: v1.result.exclusions,
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
            exclusions: v1.result.exclusions,
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
        providerConfigKey: providerContract.config_key,
        providerModel: providerContract.model,
        runSmoke: scenario.id === smokeScenarioId,
      });
      result = outcome.result;
      smokeRan = outcome.smokeRan;
    }
    result.elapsed_ms = Date.now() - startedAt;
    return { result, smokeRan };
  });
  const orderedOutcomes = outcomes.toSorted((left, right) =>
    left.result.id.localeCompare(right.result.id),
  );
  const results = orderedOutcomes.map((outcome) => outcome.result);
  const smokeRan = orderedOutcomes.some((outcome) => outcome.smokeRan);
  for (const result of results) {
    if (result.status === "failed") {
      console.error(`FAIL ${result.id}: ${result.failure?.message}`);
      console.error(`forensics: ${result.forensic_dir}`);
    }
  }
  if (!smokeRan && scenarios.some((scenario) => scenario.execution === "shared-server")) {
    throw new Error("shared-server smoke did not run");
  }
  const report = validated.matrix
    ? reportTable(validated.matrix, results, config.selector)
    : {
        text: results
          .map(
            (result) =>
              `${result.id} | ${result.status}${result.failure ? ` | ${result.failure.code}` : ""} | ${result.elapsed_ms ?? 0}ms`,
          )
          .join("\n"),
        failed: results.some((result) => result.status === "failed" && !result.issue),
      };
  console.log(report.text);
  console.log(`required check: ${validated.requiredCheckStatus}`);
  await writeFile(
    join(config.runRoot, "report.json"),
    `${JSON.stringify({ pinned_host_version: pinnedHostVersion, results }, null, 2)}\n`,
  );
  process.exit(report.failed ? 1 : 0);
}

main().catch((error) => {
  console.error(error instanceof Error ? (error.stack ?? error.message) : String(error));
  if (error instanceof HarnessError && error.code === "contract_uncaptured") {
    console.error(`HarnessError details: ${JSON.stringify(error.details)}`);
  }
  // Exit now rather than setting an exit code: hosts, mock servers and watchers
  // from scenarios still in flight keep the event loop alive, so a harness
  // error would otherwise hang until the CI job's timeout kills it.
  process.exit(1);
});
