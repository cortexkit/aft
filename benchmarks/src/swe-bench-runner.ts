#!/usr/bin/env bun
/**
 * SWE-bench runner for AFT benchmark.
 *
 * Runs opencode on real GitHub issues from large repos (django 500K+ LOC).
 * Measures exploration efficiency: tokens, tool calls, time-to-first-edit.
 *
 * Usage:
 *   bun run benchmarks/src/swe-bench-runner.ts                    # All 10 tasks, both modes
 *   bun run benchmarks/src/swe-bench-runner.ts -n 1 -M with-aft   # 1 run, AFT only
 *   bun run benchmarks/src/swe-bench-runner.ts -t django__django-11179 -v
 */
import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
  renameSync,
  copyFileSync,
  unlinkSync,
} from "fs";
import { join, resolve } from "path";
import { parseArgs } from "util";
import { homedir } from "os";
import type { BenchMode, RunResult } from "./config";
import { extractMetrics, parseSessionId } from "./metrics";
import { generateReport, printReport } from "./reporter";

interface SWEBenchTask {
  instance_id: string;
  repo: string;
  language?: string;
  base_commit: string;
  problem_statement: string;
  hints_text: string;
  version: string;
  FAIL_TO_PASS: string;
  PASS_TO_PASS: string;
}

/** Run a command with timeout. */
async function exec(
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

/** Clone and checkout repo at specific commit. */
async function setupRepo(
  task: SWEBenchTask,
  workDir: string,
  cacheDir: string,
  verbose: boolean,
): Promise<boolean> {
  const repoUrl = `https://github.com/${task.repo}.git`;
  const cacheRepo = join(cacheDir, task.repo.replace("/", "__"));

  // Clone to cache if not exists (bare clone for speed)
  if (!existsSync(cacheRepo)) {
    if (verbose) console.log(`    Cloning ${task.repo} to cache...`);
    const cloneResult = await exec(
      ["git", "clone", "--bare", repoUrl, cacheRepo],
      { timeout: 300_000 },
    );
    if (cloneResult.exitCode !== 0) {
      console.error(`    Failed to clone: ${cloneResult.stderr.slice(-200)}`);
      return false;
    }
  }

  // Create working copy from cache at the right commit
  if (verbose) console.log(`    Checking out ${task.base_commit.slice(0, 8)}...`);
  const result = await exec(
    ["git", "clone", cacheRepo, workDir],
    { timeout: 120_000 },
  );
  if (result.exitCode !== 0) {
    console.error(`    Failed to clone from cache: ${result.stderr.slice(-200)}`);
    return false;
  }

  const checkout = await exec(
    ["git", "checkout", task.base_commit],
    { cwd: workDir, timeout: 30_000 },
  );
  if (checkout.exitCode !== 0) {
    // Fetch the commit if not in bare clone
    await exec(["git", "fetch", "origin", task.base_commit], {
      cwd: workDir,
      timeout: 120_000,
    });
    const retry = await exec(["git", "checkout", task.base_commit], {
      cwd: workDir,
      timeout: 30_000,
    });
    if (retry.exitCode !== 0) {
      console.error(`    Failed to checkout: ${retry.stderr.slice(-200)}`);
      return false;
    }
  }

  return true;
}

/** Run opencode on a SWE-bench task. */
async function runTask(opts: {
  task: SWEBenchTask;
  mode: BenchMode;
  model: string;
  runIndex: number;
  timeout: number;
  cacheDir: string;
  verbose: boolean;
}): Promise<RunResult> {
  const { task, mode, model, runIndex, timeout, cacheDir, verbose } = opts;
  const workDir = join("/tmp", `aft-swe-${task.instance_id}-${mode}-${runIndex}`);

  // Clean previous run
  if (existsSync(workDir)) {
    await exec(["rm", "-rf", workDir], {});
  }

  // Setup repo
  const setupOk = await setupRepo(task, workDir, cacheDir, verbose);
  if (!setupOk) {
    return {
      exercise: task.instance_id,
      mode,
      runIndex,
      success: false,
      testsPassed: false,
      wallTimeMs: 0,
      agentTimeMs: 0,
      tokens: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, total: 0 },
      toolCalls: 0,
      failedToolCalls: 0,
      failedToolCallsByName: {},
      toolCallsByName: {},
      sessionId: null,
      error: "Failed to setup repo",
    };
  }

  // Config via OPENCODE_CONFIG env var (pre-created files in benchmarks/configs/)
  const configPath = resolve(import.meta.dir, "..", "configs", `${mode}.json`);
  // Build prompt
  const prompt = [
    `You are working on the ${task.repo} repository.`,
    "A GitHub issue has been reported. Your task is to find the relevant code and create a fix.",
    "",
    "## Issue Description",
    "",
    task.problem_statement,
    "",
    "## Instructions",
    "",
    mode =='with-aft' ?
        "1. Explore the codebase to find the relevant files and functions, you can use aft_outline, aft_zoom and other aft_* tools where you deem necessary." :
        "1. Explore the codebase to find the relevant files and functions",
    "2. Understand the root cause of the issue",
    "3. Make the minimal code change to fix the issue",
    "4. Do NOT run tests (the test environment is not set up)",
    "5. Do NOT modify test files",
  ].join("\n");

  const startTime = Date.now();
  let sessionId: string | null = null;
  let agentSuccess = false;
  let error: string | undefined;

  try {
    if (verbose) console.log(`    Running opencode (${mode})...`);

    const result = await exec(
      ["opencode", "run", "--print-logs", "-m", model, prompt],
      { cwd: workDir, timeout, env: { OPENCODE_CONFIG: configPath } },
    );

    agentSuccess = result.exitCode === 0;
    sessionId = parseSessionIdFromLogs(result.stderr);

    if (verbose) {
      if (sessionId) console.log(`    Session: ${sessionId}`);
      if (!agentSuccess) {
        console.log(`    Exit code: ${result.exitCode}`);
        const lines = result.stderr.trim().split("\n").slice(-3).join("\n");
        console.log(`    ${lines}`);
      }
    }
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
    if (verbose) console.log(`    Error: ${error}`);
  }

  const wallTimeMs = Date.now() - startTime;

  // Capture the patch (git diff from base_commit)
  let modelPatch = "";
  try {
    const diff = await exec(["git", "diff", task.base_commit], { cwd: workDir });
    modelPatch = diff.stdout;
  } catch {}

  // Extract metrics
  let tokens = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, total: 0 };
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
      if (verbose) console.log(`    Metrics extraction failed: ${e}`);
    }
  }

  // Save the patch for optional SWE-bench evaluation
  if (modelPatch) {
    const patchDir = resolve(import.meta.dir, "..", "results", "patches");
    mkdirSync(patchDir, { recursive: true });
    writeFileSync(
      join(patchDir, `${task.instance_id}-${mode}-${runIndex}.patch`),
      modelPatch,
    );
  }

  // Clean up workdir (keep cache)
  try {
    await exec(["rm", "-rf", workDir], {});
  } catch {}

  return {
    exercise: task.instance_id,
    mode,
    runIndex,
    success: agentSuccess && modelPatch.length > 0,
    testsPassed: false, // we don't run tests, SWE-bench harness does that
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

function parseSessionIdFromLogs(stderr: string): string | null {
  const match = stderr.match(/sessionID=(ses_[a-zA-Z0-9]+)/);
  return match ? match[1] : null;
}

// --- Global config backup (same as exercism runner) ---
const GLOBAL_CONFIG_PATHS = [
  "~/.config/opencode/config.json",
  "~/.config/opencode/opencode.json",
  "~/.config/opencode/opencode.jsonc"
].map((p) => p.replace("~", homedir()));

function backupGlobalConfigs(): { backedUp: string[]; restore: () => void } {
  const backedUp: string[] = [];
  for (const p of GLOBAL_CONFIG_PATHS) {
    if (existsSync(p)) {
      const bak = p + ".bench-backup";
      try { renameSync(p, bak); backedUp.push(p); } catch {
        try { copyFileSync(p, bak); unlinkSync(p); backedUp.push(p); } catch {}
      }
    }
  }
  return {
    backedUp,
    restore: () => {
      for (const p of backedUp) {
        const bak = p + ".bench-backup";
        try { renameSync(bak, p); } catch {
          try { copyFileSync(bak, p); unlinkSync(bak); } catch {}
        }
      }
    },
  };
}

// --- Main ---
async function main() {
  const { values } = parseArgs({
    options: {
      model: { type: "string", short: "m", default: "anthropic/claude-sonnet-4-6" },
      runs: { type: "string", short: "n", default: "1" },
      task: { type: "string", short: "t", multiple: true },
      timeout: { type: "string", default: "600000" }, // 10 min per task
      mode: { type: "string", short: "M", default: "both" },
      dataset: { type: "string", short: "d", default: "swe-bench-django-10.json" },
      language: { type: "string", short: "l" },
      verbose: { type: "boolean", short: "v" },
      help: { type: "boolean", short: "h" },
    },
    strict: true,
  });

  if (values.help) {
    console.log(`
SWE-bench Django Benchmark (AFT exploration efficiency)

Usage: bun run benchmarks/src/swe-bench-runner.ts [options]

Options:
  -m, --model <model>   Model (default: anthropic/claude-sonnet-4-6)
  -n, --runs <count>    Runs per task per mode (default: 1)
  -t, --task <id>       Specific task(s) (can repeat)
  -M, --mode <mode>     "with-aft", "without-aft", or "both" (default: both)
  -d, --dataset <file>  Dataset JSON file (default: swe-bench-django-10.json)
  -l, --language <lang> Filter by language (typescript, rust, go, c, javascript)
  -v, --verbose         Verbose logging
  --timeout <ms>        Timeout per task (default: 600000 = 10min)
`);
    process.exit(0);
  }

  const model = values.model ?? "anthropic/claude-sonnet-4-6";
  const runs = parseInt(values.runs ?? "1", 10);
  const timeout = parseInt(values.timeout ?? "600000", 10);
  const verbose = values.verbose ?? false;

  let modes: BenchMode[] = ["with-aft", "without-aft"];
  if (values.mode === "with-aft") modes = ["with-aft"];
  else if (values.mode === "without-aft") modes = ["without-aft"];

  // Load tasks
  const datasetFile = values.dataset ?? "swe-bench-django-10.json";
  const tasksPath = resolve(import.meta.dir, "..", datasetFile);
  const allTasks: SWEBenchTask[] = JSON.parse(readFileSync(tasksPath, "utf-8"));
  const taskFilter = values.task as string[] | undefined;
  const langFilter = values.language as string | undefined;
  let tasks = taskFilter?.length
    ? allTasks.filter((t) => taskFilter.includes(t.instance_id))
    : allTasks;
  if (langFilter) {
    tasks = tasks.filter((t) => t.language === langFilter);
  }

  if (tasks.length === 0) {
    console.error("No tasks matched. Available:", allTasks.map((t) => t.instance_id).join(", "));
    process.exit(1);
  }

  // Repo cache dir
  const cacheDir = resolve(import.meta.dir, "..", "cache", "repos");
  mkdirSync(cacheDir, { recursive: true });

  // Backup global configs
  const backup = backupGlobalConfigs();
  const cleanup = () => { backup.restore(); process.exit(1); };
  process.on("SIGINT", cleanup);
  process.on("SIGTERM", cleanup);

  const totalRuns = tasks.length * modes.length * runs;
  console.log(`\n  SWE-bench Django Benchmark`);
  console.log(`  Model: ${model}`);
  const langInfo = langFilter ? ` (${langFilter} only)` : "";
  console.log(`  Tasks: ${tasks.length}${langInfo}`);
  console.log(`  Modes: ${modes.join(", ")}`);
  console.log(`  Runs: ${runs} per task per mode`);
  console.log(`  Total: ${totalRuns} runs`);
  if (backup.backedUp.length > 0) {
    console.log(`  Backed up ${backup.backedUp.length} global config(s)`);
  }
  console.log();

  const results: RunResult[] = [];
  let completed = 0;

  for (const task of tasks) {
    for (const mode of modes) {
      for (let i = 0; i < runs; i++) {
        completed++;
        console.log(`[${completed}/${totalRuns}] ${task.instance_id} (${mode}, run ${i + 1}/${runs})`);

        const result = await runTask({
          task,
          mode,
          model,
          runIndex: i,
          timeout,
          cacheDir,
          verbose,
        });

        results.push(result);

        const tokens = result.tokens.total.toLocaleString();
        const time = (result.wallTimeMs / 1000).toFixed(1);
        const patchStatus = result.success ? "✅ patch" : "❌ no patch";
        const failedStr = result.failedToolCalls > 0 ? ` failed=${result.failedToolCalls}` : "";
        console.log(`  ${patchStatus} tokens=${tokens} time=${time}s tools=${result.toolCalls}${failedStr}`);
      }
    }
  }

  // Report
  const report = generateReport(results, { model, runs, exercises: tasks.map((t) => t.instance_id) });
  printReport(report);

  // Save
  const resultsDir = resolve(import.meta.dir, "..", "results");
  mkdirSync(resultsDir, { recursive: true });
  const ts = new Date().toISOString().replace(/[:.]/g, "-");
  const filepath = join(resultsDir, `swe-bench-${ts}.json`);
  await Bun.write(filepath, JSON.stringify(report, null, 2));
  console.log(`\nResults saved to: ${filepath}`);

  // Save predictions in SWE-bench format for optional evaluation
  const predictions = results
    .filter((r) => r.success)
    .map((r) => {
      const patchPath = join(resultsDir, "patches", `${r.exercise}-${r.mode}-${r.runIndex}.patch`);
      const patch = existsSync(patchPath) ? readFileSync(patchPath, "utf-8") : "";
      return {
        instance_id: r.exercise,
        model_name_or_path: `opencode-${r.mode}`,
        model_patch: patch,
      };
    });
  if (predictions.length > 0) {
    const predPath = join(resultsDir, `predictions-${ts}.jsonl`);
    await Bun.write(predPath, predictions.map((p) => JSON.stringify(p)).join("\n") + "\n");
    console.log(`Predictions saved to: ${predPath} (run swebench eval harness on this)`);
  }

  // Restore configs
  backup.restore();
  if (backup.backedUp.length > 0) {
    console.log(`Restored ${backup.backedUp.length} global config(s)`);
  }
}

main().catch((e) => {
  console.error("Benchmark failed:", e);
  process.exit(1);
});
