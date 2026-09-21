import { type ChildProcess, spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { Readable } from "node:stream";

import { applyHandoff, type HostCliContract } from "./contracts.js";
import { fail } from "./errors.js";
import type { ApiControlPlan, ScenarioDefinition } from "./types.js";
import { asRecord, type CommandOutput } from "./util.js";
import { ProcessObserver } from "./process-observer.js";

interface CapturedChild {
  child: ChildProcess;
  command: string[];
  cwd: string;
  stdout: string;
  stderr: string;
  timedOut: boolean;
  /** Resolves once both pipes have ended, so nothing written at exit is lost. */
  stdioClosed: Promise<void>;
}

/**
 * How long to keep reading a child's pipes after it has exited.
 *
 * `exit` fires when the process is gone, not when everything it wrote has been
 * read: a host that prints a diagnostic and dies immediately can lose that
 * last chunk, which is the one worth having. The cap only matters when a pipe
 * stays open because some other process inherited it; the normal case resolves
 * as soon as the pipes end.
 */
const STDIO_FLUSH_TIMEOUT_MS = 2_000;

function streamEnded(stream: Readable | null | undefined): Promise<void> {
  if (!stream || stream.readableEnded || stream.destroyed) return Promise.resolve();
  return new Promise<void>((resolvePromise) => {
    const finish = () => resolvePromise();
    stream.once("end", finish);
    stream.once("close", finish);
    stream.once("error", finish);
  });
}

export interface SharedServerHandle {
  child: ChildProcess;
  endpoint: string;
  password: string;
  /** The service registration the host published for this server. */
  registration: ServiceRegistration;
  stdout: () => string;
  stderr: () => string;
}

/**
 * What `opencode serve --service` writes so other processes can find it.
 *
 * This is the file `discover()` in @opencode/client/service reads, at
 * `$XDG_STATE_HOME/opencode/service.json`.
 */
export interface ServiceRegistration {
  url: string;
  password: string;
  pid: number;
  version?: string;
}

export function serviceRegistrationPath(stateRoot: string): string {
  return join(stateRoot, "opencode", "service.json");
}

async function readServiceRegistration(path: string): Promise<ServiceRegistration | undefined> {
  const text = await readFile(path, "utf8").catch(() => undefined);
  if (text === undefined) return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return undefined;
  }
  const record = asRecord(parsed);
  if (
    typeof record?.url !== "string" ||
    typeof record.password !== "string" ||
    typeof record.pid !== "number"
  ) {
    return undefined;
  }
  return {
    url: record.url,
    password: record.password,
    pid: record.pid,
    version: typeof record.version === "string" ? record.version : undefined,
  };
}

function spawnCaptured(
  executable: string,
  args: string[],
  cwd: string,
  env: NodeJS.ProcessEnv,
): CapturedChild {
  const captured: CapturedChild = {
    child: undefined as unknown as ChildProcess,
    command: [executable, ...args],
    cwd,
    stdout: "",
    stderr: "",
    timedOut: false,
    stdioClosed: Promise.resolve(),
  };
  captured.child = spawn(executable, args, {
    cwd,
    env,
    detached: process.platform !== "win32",
    stdio: ["ignore", "pipe", "pipe"],
  });
  captured.child.stdout?.on("data", (chunk) => {
    captured.stdout += String(chunk);
  });
  captured.child.stderr?.on("data", (chunk) => {
    captured.stderr += String(chunk);
  });
  captured.stdioClosed = Promise.all([
    streamEnded(captured.child.stdout),
    streamEnded(captured.child.stderr),
  ]).then(() => undefined);
  return captured;
}

async function waitCaptured(captured: CapturedChild, timeoutMs: number): Promise<CommandOutput> {
  const result =
    captured.child.exitCode !== null || captured.child.signalCode !== null
      ? { code: captured.child.exitCode, signal: captured.child.signalCode }
      : await new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(
          (resolvePromise, reject) => {
            let timer: NodeJS.Timeout | undefined;
            const finish = (code: number | null, signal: NodeJS.Signals | null) => {
              if (timer) clearTimeout(timer);
              resolvePromise({ code, signal });
            };
            captured.child.once("error", reject);
            captured.child.once("exit", finish);
            timer = setTimeout(() => {
              captured.timedOut = true;
              if (captured.child.pid && process.platform !== "win32") {
                try {
                  process.kill(-captured.child.pid, "SIGKILL");
                } catch {}
              } else {
                captured.child.kill("SIGKILL");
              }
            }, timeoutMs);
            timer.unref();
          },
        );
  await Promise.race([captured.stdioClosed, Bun.sleep(STDIO_FLUSH_TIMEOUT_MS)]);
  return {
    command: captured.command,
    cwd: captured.cwd,
    exit_code: result.code,
    signal: result.signal,
    stdout: captured.stdout,
    stderr: captured.stderr,
    timed_out: captured.timedOut,
  };
}

/**
 * Start the shared host for a scenario as a registered background service.
 *
 * `--service` is what makes the host publish its registration file, and that
 * file is the only thing `discover()` in @opencode/client/service looks at. A
 * plain `serve` listens on the same API but registers nothing, so a plugin
 * running inside it cannot find the service it is part of and can never raise
 * an interactive permission prompt. Starting it the way a real installation
 * does is what puts that path under test.
 *
 * The registration is also where the password now comes from: `--service`
 * stops printing `server password` on stdout and writes it into the file
 * instead. Waiting for the file therefore doubles as proof that the service
 * registered at all.
 */
export async function startSharedServer(options: {
  executable: string;
  cwd: string;
  env: NodeJS.ProcessEnv;
  processObserver: ProcessObserver;
  /** XDG state root for this scenario; the registration is written under it. */
  stateRoot: string;
  timeoutMs?: number;
}): Promise<SharedServerHandle> {
  const args = ["serve", "--hostname", "127.0.0.1", "--port", "0", "--service", "--print-logs"];
  const captured = spawnCaptured(options.executable, args, options.cwd, options.env);
  options.processObserver.trackChild("opencode2-serve", captured.child, "host");
  const registrationPath = serviceRegistrationPath(options.stateRoot);
  const deadline = Date.now() + (options.timeoutMs ?? 30_000);
  let endpoint: string | undefined;
  let registration: ServiceRegistration | undefined;
  while (Date.now() < deadline && captured.child.exitCode === null) {
    const output = `${captured.stdout}\n${captured.stderr}`;
    endpoint = output.match(/server listening on (http:\/\/127\.0\.0\.1:\d+)/)?.[1];
    registration = await readServiceRegistration(registrationPath);
    if (endpoint && registration && registration.url === endpoint) break;
    await Bun.sleep(50);
  }
  if (!endpoint || !registration || registration.url !== endpoint) {
    captured.child.kill("SIGTERM");
    fail(
      "host_failed",
      "shared server did not publish a service registration matching its endpoint",
      {
        endpoint,
        registration,
        registration_path: registrationPath,
        stdout: captured.stdout,
        stderr: captured.stderr,
      },
      true,
    );
  }
  return {
    child: captured.child,
    endpoint,
    password: registration.password,
    registration,
    stdout: () => captured.stdout,
    stderr: () => captured.stderr,
  };
}

function attachedCommand(
  baseArgs: string[],
  env: NodeJS.ProcessEnv,
  endpoint: string,
  password: string | undefined,
  endpointHandoff: HostCliContract["endpoint_handoff"]["run"],
  passwordHandoff: HostCliContract["password_handoff"]["run"],
): { args: string[]; env: NodeJS.ProcessEnv } {
  const args = [...baseArgs];
  const attachedEnv = { ...env };
  applyHandoff(endpointHandoff, endpoint, args, attachedEnv);
  if (password !== undefined) {
    applyHandoff(passwordHandoff, password, args, attachedEnv);
  } else if (passwordHandoff.kind === "env") {
    delete attachedEnv[passwordHandoff.name];
  } else if (passwordHandoff.kind === "header") {
    const headers = attachedEnv.OPENCODE_API_HEADERS
      ? (JSON.parse(attachedEnv.OPENCODE_API_HEADERS) as Record<string, unknown>)
      : {};
    delete headers[passwordHandoff.name];
    attachedEnv.OPENCODE_API_HEADERS = JSON.stringify(headers);
  }
  return { args, env: attachedEnv };
}

export function startScenarioClient(options: {
  executable: string;
  scenario: ScenarioDefinition;
  cwd: string;
  env: NodeJS.ProcessEnv;
  processObserver: ProcessObserver;
  contract?: HostCliContract;
  server?: SharedServerHandle;
  hostGeneration?: "v1" | "v2";
  model?: string;
}): { child: ChildProcess; wait: (timeoutMs?: number) => Promise<CommandOutput> } {
  const baseArgs = ["run"];
  let args: string[];
  let env: NodeJS.ProcessEnv;
  if (options.scenario.execution === "standalone") {
    // `--standalone` is a V2 flag; a V1 `run` already starts its own host.
    // Both take `--print-logs`, and that flag is the only way the host's own
    // diagnostics reach the harness: without it a host that dies during
    // startup exits 1 with nothing but an opaque "Unexpected server error"
    // line on stdout and an empty stderr log.
    args =
      options.hostGeneration === "v1"
        ? [...baseArgs, "--print-logs"]
        : [...baseArgs, "--standalone", "--print-logs"];
    env = { ...options.env };
  } else {
    if (!options.contract || !options.server)
      throw new Error("shared scenario requires host contract and server");
    const attached = attachedCommand(
      baseArgs,
      options.env,
      options.server.endpoint,
      options.server.password,
      options.contract.endpoint_handoff.run,
      options.contract.password_handoff.run,
    );
    args = attached.args;
    env = attached.env;
  }
  args.push("--format", "json", "--model", options.model ?? options.scenario.model ?? "openai/mock-model");
  if (options.scenario.auto) args.push("--auto");
  args.push(options.scenario.prompt);
  const captured = spawnCaptured(options.executable, args, options.cwd, env);
  options.processObserver.trackChild("opencode2-run", captured.child, "host");
  return {
    child: captured.child,
    wait: (timeoutMs = 120_000) => waitCaptured(captured, timeoutMs),
  };
}

export async function runApiCommand(options: {
  executable: string;
  cwd: string;
  env: NodeJS.ProcessEnv;
  contract: HostCliContract;
  endpoint: string;
  password?: string;
  method: string;
  path: string;
  body?: unknown;
  timeoutMs?: number;
}): Promise<CommandOutput> {
  const baseArgs = ["api"];
  const attached = attachedCommand(
    baseArgs,
    options.env,
    options.endpoint,
    options.password,
    options.contract.endpoint_handoff.api,
    options.contract.password_handoff.api,
  );
  attached.args.push(options.method, options.path);
  // `opencode api` names its body flag `--data`; `--body` is rejected as an
  // unrecognised flag before the request is made, which turns a control into a
  // silent no-op. The spelling is read from the captured CLI contract.
  if (options.body !== undefined) {
    attached.args.push(options.contract.request_body_flag, JSON.stringify(options.body));
  }
  const captured = spawnCaptured(options.executable, attached.args, options.cwd, attached.env);
  return waitCaptured(captured, options.timeoutMs ?? 10_000);
}

const CONTROL_PATH_PLACEHOLDER = /\{\{(permission_id|session_id|task_id)(?::([^{}]+))?\}\}/g;

export type ControlPathValues = Readonly<Record<string, string>>;

export function interpolateControlPath(path: string, values: ControlPathValues): string {
  const rendered = path.replace(
    CONTROL_PATH_PLACEHOLDER,
    (placeholder, kind: string, qualifier: string | undefined) => {
      const key = qualifier ? `${kind}:${qualifier}` : kind;
      const value = values[key];
      if (!value) {
        fail("scenario_invalid", `control path placeholder has no value: ${placeholder}`, {
          path,
          placeholder,
        });
      }
      return encodeURIComponent(value);
    },
  );
  const unresolved = rendered.match(/\{\{[^{}]+\}\}/)?.[0];
  if (unresolved) {
    fail("scenario_invalid", `unsupported control path placeholder: ${unresolved}`, {
      path,
      placeholder: unresolved,
    });
  }
  return rendered;
}

export async function runApiControl(
  plan: ApiControlPlan,
  options: Omit<Parameters<typeof runApiCommand>[0], "method" | "path" | "body"> & {
    controlPathValues?: ControlPathValues;
  },
): Promise<CommandOutput> {
  if (plan.delay_ms) await Bun.sleep(plan.delay_ms);
  const result = await runApiCommand({
    ...options,
    method: plan.method,
    path: interpolateControlPath(plan.path, options.controlPathValues ?? {}),
    body: plan.body,
  });
  const expected = plan.expected_status ?? 0;
  if (result.exit_code !== expected) {
    const detail = `${result.stdout}\n${result.stderr}`.trim();
    fail(
      "host_failed",
      `control ${plan.id} exited ${result.exit_code}, expected ${expected}${detail ? `: ${detail}` : ""}`,
      {
        control: plan,
        output: result,
      },
    );
  }
  const visible = `${result.stdout}\n${result.stderr}`;
  if (plan.expected_stdout_pattern && !new RegExp(plan.expected_stdout_pattern).test(visible)) {
    const detail = visible.trim();
    fail(
      "host_failed",
      `control ${plan.id} did not match expected output${detail ? `: ${detail}` : ""}`,
      {
        control: plan,
        output: result,
      },
    );
  }
  if (plan.forbidden_stdout_pattern && new RegExp(plan.forbidden_stdout_pattern).test(visible)) {
    fail("host_failed", `control ${plan.id} matched forbidden output`, {
      control: plan,
      output: result,
    });
  }
  return result;
}

export async function runSharedServerSmoke(options: {
  executable: string;
  cwd: string;
  env: NodeJS.ProcessEnv;
  contract: HostCliContract;
  server: SharedServerHandle;
}): Promise<Record<string, CommandOutput>> {
  const smoke = options.contract.shared_server_smoke;
  const correct = await runApiCommand({
    ...options,
    endpoint: options.server.endpoint,
    password: options.server.password,
    method: smoke.method,
    path: smoke.path,
  });
  if (correct.exit_code !== (smoke.expected_status ?? 0)) {
    fail("host_failed", "shared-server smoke correct attach failed", { output: correct }, true);
  }
  const wrongPassword = await runApiCommand({
    ...options,
    endpoint: options.server.endpoint,
    password: `${options.server.password}-WRONG`,
    method: smoke.method,
    path: smoke.path,
  });
  if (wrongPassword.exit_code === 0) {
    fail("host_failed", "shared-server smoke accepted WRONG password", {}, true);
  }
  const missingPassword = await runApiCommand({
    ...options,
    endpoint: options.server.endpoint,
    password: undefined,
    method: smoke.method,
    path: smoke.path,
  });
  if (missingPassword.exit_code === 0) {
    fail("host_failed", "shared-server smoke accepted MISSING password", {}, true);
  }
  const badEndpoint = await runApiCommand({
    ...options,
    endpoint: "http://127.0.0.1:1",
    password: options.server.password,
    method: smoke.method,
    path: smoke.path,
  });
  if (badEndpoint.exit_code === 0) {
    fail("host_failed", "shared-server smoke accepted bad endpoint", {}, true);
  }
  return {
    correct,
    wrong_password: wrongPassword,
    missing_password: missingPassword,
    bad_endpoint: badEndpoint,
  };
}
