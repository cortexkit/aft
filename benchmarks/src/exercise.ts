import { join, resolve } from "path";
import {
  mkdirSync,
  rmSync,
  existsSync,
  readFileSync,
  writeFileSync,
  cpSync,
} from "fs";
import type { BenchMode, RunResult } from "./config";
import { extractMetrics, parseSessionId } from "./metrics";


/** Run a command with timeout, returning stdout/stderr/exitCode. */
async function execWithTimeout(
  cmd: string[],
  opts: { cwd?: string; timeout?: number; env?: Record<string, string> },
): Promise<{ stdout: string; stderr: string; exitCode: number }> {
  const proc = Bun.spawn(cmd, {
    cwd: opts.cwd,
    stdout: "pipe",
    stderr: "pipe",
    env: opts.env ? { ...process.env, ...opts.env } : undefined,
  });

  let timedOut = false;
  const timer = opts.timeout
    ? setTimeout(() => {
        timedOut = true;
        proc.kill();
      }, opts.timeout)
    : null;

  const [stdout, stderr] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);

  const exitCode = await proc.exited;
  if (timer) clearTimeout(timer);

  return timedOut
    ? { stdout, stderr: stderr + "\n[TIMEOUT]", exitCode: 124 }
    : { stdout, stderr, exitCode };
}

/**
 * Run a single exercise with a given configuration.
 */
export async function runExercise(opts: {
  exercise: string;
  exercisesPath: string;
  mode: BenchMode;
  model: string;
  runIndex: number;
  timeout: number;
  verbose: boolean;
}): Promise<RunResult> {
  const { exercise, exercisesPath, mode, model, runIndex, timeout, verbose } =
    opts;

  const srcDir = join(exercisesPath, exercise);
  const workDir = join("/tmp", `aft-bench-${exercise}-${mode}-${runIndex}`);

  // Clean and copy exercise to temp dir
  if (existsSync(workDir)) rmSync(workDir, { recursive: true });
  mkdirSync(workDir, { recursive: true });
  cpSync(srcDir, workDir, { recursive: true });

  // Initialize git repo (opencode needs it)
  await execWithTimeout(["git", "init"], { cwd: workDir });
  await execWithTimeout(["git", "add", "-A"], { cwd: workDir });
  await execWithTimeout(
    ["git", "commit", "-m", "init", "--allow-empty"],
    { cwd: workDir, env: { GIT_AUTHOR_NAME: "bench", GIT_AUTHOR_EMAIL: "bench@test", GIT_COMMITTER_NAME: "bench", GIT_COMMITTER_EMAIL: "bench@test" } },
  );

  // Config via OPENCODE_CONFIG env var (pre-created files in benchmarks/configs/)
  const configPath = resolve(import.meta.dir, "..", "configs", `${mode}.json`);

  const instructions = buildInstructions(srcDir, exercise);

  const startTime = Date.now();
  let sessionId: string | null = null;
  let agentSuccess = false;
  let testsPassed = false;
  let error: string | undefined;

  try {
    if (verbose)
      console.log(`    Running opencode (${mode}) for ${exercise}...`);

    // Run opencode from the exercise directory with --print-logs to capture session ID
    const agentResult = await execWithTimeout(
      ["opencode", "run", "--print-logs", "-m", model, instructions],
      { cwd: workDir, timeout, env: { OPENCODE_CONFIG: configPath } },
    );

    agentSuccess = agentResult.exitCode === 0;

    // Extract session ID from log lines (e.g., "sessionID=ses_xxx")
    sessionId = parseSessionIdFromLogs(agentResult.stderr);

    if (verbose) {
      if (sessionId) console.log(`    Session: ${sessionId}`);
      if (!agentSuccess) {
        console.log(`    Agent exit code: ${agentResult.exitCode}`);
        // Show last few lines of stderr for debugging
        const stderrLines = agentResult.stderr.trim().split("\n");
        const lastLines = stderrLines.slice(-5).join("\n");
        console.log(`    Stderr (last 5 lines):\n${lastLines}`);
        if (agentResult.stderr.includes("[TIMEOUT]")) {
          console.log(`    TIMED OUT after ${timeout}ms`);
        }
      }
    }

    // Run tests
    if (verbose) console.log(`    Running tests for ${exercise}...`);
    const testResult = await execWithTimeout(
      ["corepack", "yarn", "test"],
      { cwd: workDir, timeout: 60_000 },
    );

    testsPassed = testResult.exitCode === 0;
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
    if (verbose) console.log(`    Error: ${error}`);
  }

  const wallTimeMs = Date.now() - startTime;

  // Extract metrics from session
  let tokens = {
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    reasoning: 0,
    total: 0,
  };
  let toolCalls = 0;
  let failedToolCalls = 0;
  let toolCallsByName: Record<string, number> = {};
  let failedToolCallsByName: Record<string, number> = {};
  let agentTimeMs = 0;

  if (sessionId) {
    try {
      const metrics = await extractMetrics(sessionId);
      tokens = metrics.tokens;
      toolCalls = metrics.toolCalls;
      failedToolCalls = metrics.failedToolCalls;
      toolCallsByName = metrics.toolCallsByName;
      failedToolCallsByName = metrics.failedToolCallsByName;
      agentTimeMs = metrics.agentTimeMs;
    } catch (e) {
      if (verbose) console.log(`    Failed to extract metrics: ${e}`);
    }
  }
      if (verbose) console.log(`    Failed to extract metrics: ${e}`);
    }
  }

  // Clean up
  try {
    rmSync(workDir, { recursive: true });
  } catch {}

  return {
    exercise,
    mode,
    runIndex,
    success: agentSuccess && testsPassed,
    testsPassed,
    wallTimeMs,
    agentTimeMs,
    tokens,
    toolCalls,
    failedToolCalls,
    failedToolCallsByName,
    toolCallsByName,
    sessionId,
    error,
  };
}

function writeOpenCodeConfig(workDir: string, mode: BenchMode): void {
  // opencode looks for .opencode/config.json in the project root
  const configDir = join(workDir, ".opencode");
  mkdirSync(configDir, { recursive: true });

  const config: Record<string, unknown> = {
    agent: {
      explore: { disabled: true },
      general: { disabled: true },
    },
  };

  if (mode === "with-aft") {
    const pluginPath = resolve(import.meta.dir, "..", "..", "packages", "opencode-plugin");
    config.plugins = [pluginPath];
  }
  // without-aft: no plugins, just disabled agents
  writeFileSync(
    join(configDir, "config.json"),
    JSON.stringify(config, null, 2),
  );
}

function buildInstructions(
  exerciseDir: string,
  exercise: string,
): string {
  let instructions =
    "Implement the solution for this TypeScript exercise. ";
  instructions +=
    "The source file has a stub function that throws an error. ";
  instructions +=
    "Replace it with a working implementation that passes all tests. ";
  instructions += "Do NOT modify the test file. ";
  instructions +=
    "Run the tests with `corepack yarn test` to verify your solution.\n\n";

  // Add exercise-specific instructions
  const docsPath = join(exerciseDir, ".docs", "instructions.md");
  if (existsSync(docsPath)) {
    instructions += readFileSync(docsPath, "utf-8");
  }

  // Add CLAUDE.md environment if it exists
  const claudePath = join(exerciseDir, "..", "..", "..", "CLAUDE.md");
  if (existsSync(claudePath)) {
    instructions += "\n\n" + readFileSync(claudePath, "utf-8");
  }

  return instructions;
}

/** Extract session ID from opencode --print-logs stderr output. */
function parseSessionIdFromLogs(stderr: string): string | null {
  // Look for sessionID=ses_xxx in log lines
  const match = stderr.match(/sessionID=(ses_[a-zA-Z0-9]+)/);
  return match ? match[1] : parseSessionId(stderr);
}

// removed - using project-level .opencode/config.json instead
