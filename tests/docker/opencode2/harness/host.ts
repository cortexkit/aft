import { type ChildProcess, spawn } from "node:child_process";

import { applyHandoff, type HostCliContract } from "./contracts.js";
import { fail } from "./errors.js";
import type { ApiControlPlan, ScenarioDefinition } from "./types.js";
import type { CommandOutput } from "./util.js";
import { ProcessObserver } from "./process-observer.js";

interface CapturedChild {
  child: ChildProcess;
  command: string[];
  cwd: string;
  stdout: string;
  stderr: string;
  timedOut: boolean;
}

export interface SharedServerHandle {
  child: ChildProcess;
  endpoint: string;
  password: string;
  stdout: () => string;
  stderr: () => string;
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

export async function startSharedServer(options: {
  executable: string;
  cwd: string;
  env: NodeJS.ProcessEnv;
  processObserver: ProcessObserver;
  timeoutMs?: number;
}): Promise<SharedServerHandle> {
  const args = ["serve", "--hostname", "127.0.0.1", "--port", "0", "--print-logs"];
  const captured = spawnCaptured(options.executable, args, options.cwd, options.env);
  options.processObserver.trackChild("opencode2-serve", captured.child, "host");
  const deadline = Date.now() + (options.timeoutMs ?? 30_000);
  let endpoint: string | undefined;
  let password: string | undefined;
  while (Date.now() < deadline && captured.child.exitCode === null) {
    const output = `${captured.stdout}\n${captured.stderr}`;
    endpoint = output.match(/server listening on (http:\/\/127\.0\.0\.1:\d+)/)?.[1];
    password = output.match(/server password ([^\s]+)/)?.[1];
    if (endpoint && password) break;
    await Bun.sleep(50);
  }
  if (!endpoint || !password) {
    captured.child.kill("SIGTERM");
    fail(
      "host_failed",
      "shared server did not publish endpoint and password",
      { stdout: captured.stdout, stderr: captured.stderr },
      true,
    );
  }
  return {
    child: captured.child,
    endpoint,
    password,
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
}): { child: ChildProcess; wait: (timeoutMs?: number) => Promise<CommandOutput> } {
  const baseArgs = ["run"];
  let args: string[];
  let env: NodeJS.ProcessEnv;
  if (options.scenario.execution === "standalone") {
    args = options.hostGeneration === "v1" ? [...baseArgs] : [...baseArgs, "--standalone"];
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
  args.push("--format", "json", "--model", "mock/mock-model");
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
  if (options.body !== undefined) attached.args.push("--body", JSON.stringify(options.body));
  const captured = spawnCaptured(options.executable, attached.args, options.cwd, attached.env);
  return waitCaptured(captured, options.timeoutMs ?? 10_000);
}

export async function runApiControl(
  plan: ApiControlPlan,
  options: Omit<Parameters<typeof runApiCommand>[0], "method" | "path" | "body">,
): Promise<CommandOutput> {
  if (plan.delay_ms) await Bun.sleep(plan.delay_ms);
  const result = await runApiCommand({
    ...options,
    method: plan.method,
    path: plan.path,
    body: plan.body,
  });
  const expected = plan.expected_status ?? 0;
  if (result.exit_code !== expected) {
    fail("host_failed", `control ${plan.id} exited ${result.exit_code}, expected ${expected}`, {
      control: plan,
      output: result,
    });
  }
  const visible = `${result.stdout}\n${result.stderr}`;
  if (plan.expected_stdout_pattern && !new RegExp(plan.expected_stdout_pattern).test(visible)) {
    fail("host_failed", `control ${plan.id} did not match expected output`, {
      control: plan,
      output: result,
    });
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
