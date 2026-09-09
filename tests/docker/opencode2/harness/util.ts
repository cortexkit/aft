import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { readFile } from "node:fs/promises";
import { isAbsolute, relative, resolve } from "node:path";

export function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

export async function readJson(path: string): Promise<unknown> {
  return JSON.parse(await readFile(path, "utf8")) as unknown;
}

export async function sha256File(path: string): Promise<string> {
  const hash = createHash("sha256");
  await new Promise<void>((resolvePromise, reject) => {
    const stream = createReadStream(path);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.once("error", reject);
    stream.once("end", resolvePromise);
  });
  return hash.digest("hex");
}

export function fixtureRelativePath(root: string, candidate: string): string {
  const absolute = resolve(root, candidate);
  const rel = relative(root, absolute).replaceAll("\\", "/");
  if (isAbsolute(rel) || rel === ".." || rel.startsWith("../")) {
    throw new Error(`path escapes fixture root: ${candidate}`);
  }
  return rel || ".";
}

export function pathInside(root: string, candidate: string): string {
  const rel = fixtureRelativePath(root, candidate);
  return resolve(root, rel);
}

export function createRunId(): string {
  return `${new Date().toISOString().replaceAll(/[:.]/g, "-")}-${randomUUID()}`;
}

export interface CommandOutput {
  command: string[];
  cwd: string;
  exit_code: number | null;
  signal: NodeJS.Signals | null;
  stdout: string;
  stderr: string;
  timed_out: boolean;
}

export async function runCommand(
  command: string,
  args: string[],
  options: {
    cwd: string;
    env?: NodeJS.ProcessEnv;
    timeoutMs?: number;
    stdin?: string;
  },
): Promise<CommandOutput> {
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: options.env,
    stdio: [options.stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  let timedOut = false;
  child.stdout?.on("data", (chunk) => {
    stdout += String(chunk);
  });
  child.stderr?.on("data", (chunk) => {
    stderr += String(chunk);
  });
  if (options.stdin !== undefined) child.stdin?.end(options.stdin);
  const timer = setTimeout(() => {
    timedOut = true;
    child.kill("SIGKILL");
  }, options.timeoutMs ?? 30_000);
  timer.unref();
  const result = await new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(
    (resolvePromise, reject) => {
      child.once("error", reject);
      child.once("exit", (code, signal) => resolvePromise({ code, signal }));
    },
  );
  clearTimeout(timer);
  return {
    command: [command, ...args],
    cwd: options.cwd,
    exit_code: result.code,
    signal: result.signal,
    stdout,
    stderr,
    timed_out: timedOut,
  };
}
