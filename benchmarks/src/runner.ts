#!/usr/bin/env bun
import { existsSync, mkdirSync, renameSync, copyFileSync, unlinkSync } from "fs";
import { parseArgs } from "util";
import { homedir } from "os";
import {
  DEFAULT_CONFIG,
  DEFAULT_EXERCISES,
  type BenchmarkConfig,
  type BenchMode,
  type RunResult,
} from "./config";
import { runExercise } from "./exercise";
import { generateReport, printReport } from "./reporter";

function parseCliArgs(): Partial<BenchmarkConfig> & {
  modes: BenchMode[];
} {
  const { values } = parseArgs({
    options: {
      model: { type: "string", short: "m" },
      runs: { type: "string", short: "n" },
      exercise: { type: "string", short: "e", multiple: true },
      timeout: { type: "string", short: "t" },
      verbose: { type: "boolean", short: "v" },
      mode: {
        type: "string",
        default: "both",
        short: "M",
      },
      help: { type: "boolean", short: "h" },
    },
    strict: true,
  });

  if (values.help) {
    console.log(`
AFT Benchmark Runner

Usage: bun run bench [options]

Options:
  -m, --model <model>     Model to use (default: anthropic/claude-sonnet-4-6)
  -n, --runs <count>      Runs per exercise per config (default: 5)
  -e, --exercise <name>   Specific exercise(s) to run (can repeat)
  -t, --timeout <ms>      Timeout per exercise in ms (default: 300000)
  -v, --verbose           Verbose logging
  -M, --mode <mode>       "with-aft", "without-aft", or "both" (default: both)
  -h, --help              Show this help

Examples:
  bun run bench                                   # Full benchmark
  bun run bench -e accumulate -e darts -n 3       # 2 exercises, 3 runs
  bun run bench -M with-aft -v                    # Only test with AFT, verbose
  bun run bench -m openai/gpt-4o                  # Use a different model
`);
    process.exit(0);
  }

  let modes: BenchMode[] = ["without-aft", "with-aft"];
  if (values.mode === "with-aft") modes = ["with-aft"];
  else if (values.mode === "without-aft") modes = ["without-aft"];

  return {
    model: values.model,
    runs: values.runs ? parseInt(values.runs, 10) : undefined,
    exercises: values.exercise as string[] | undefined,
    timeout: values.timeout ? parseInt(values.timeout, 10) : undefined,
    verbose: values.verbose,
    modes,
  };
}

async function main() {
  const args = parseCliArgs();

  const config: BenchmarkConfig = {
    ...DEFAULT_CONFIG,
    ...(args.model && { model: args.model }),
    ...(args.runs && { runs: args.runs }),
    ...(args.exercises?.length && { exercises: args.exercises }),
    ...(args.timeout && { timeout: args.timeout }),
    ...(args.verbose !== undefined && { verbose: args.verbose }),
  };

  const exercises =
    config.exercises.length > 0 ? config.exercises : DEFAULT_EXERCISES;

  // Verify exercises exist
  for (const ex of exercises) {
    const exPath = `${config.exercisesPath}/${ex}`;
    if (!existsSync(exPath)) {
      console.error(`Exercise not found: ${ex} (looked in ${exPath})`);
      process.exit(1);
    }
  }

  // Ensure results directory
  mkdirSync(config.resultsPath, { recursive: true });

  const totalRuns = exercises.length * args.modes.length * config.runs;

  // Backup global opencode configs to prevent merging during benchmark
  const globalConfigBackup = backupGlobalConfigs();

  // Ensure configs are restored even if killed
  const cleanup = () => {
    globalConfigBackup.restore();
    process.exit(1);
  };
  process.on("SIGINT", cleanup);
  process.on("SIGTERM", cleanup);
  console.log(`\n  AFT Benchmark`);
  console.log(`  Model: ${config.model}`);
  console.log(`  Exercises: ${exercises.length}`);
  console.log(`  Modes: ${args.modes.join(", ")}`);
  console.log(`  Runs per config: ${config.runs}`);
  console.log(`  Total runs: ${totalRuns}`);
  if (globalConfigBackup.backedUp.length > 0) {
    console.log(`  Backed up ${globalConfigBackup.backedUp.length} global config(s)`);
  }
  console.log();

  const results: RunResult[] = [];
  let completed = 0;

  for (const exercise of exercises) {
    for (const mode of args.modes) {
      for (let i = 0; i < config.runs; i++) {
        completed++;
        const progress = `[${completed}/${totalRuns}]`;
        console.log(
          `${progress} ${exercise} (${mode}, run ${i + 1}/${config.runs})`,
        );

        const result = await runExercise({
          exercise,
          exercisesPath: config.exercisesPath,
          mode,
          model: config.model,
          runIndex: i,
          timeout: config.timeout,
          verbose: config.verbose,
        });

        results.push(result);

        const status = result.success ? "✅" : "❌";
        const tokens = result.tokens.total.toLocaleString();
        const time = (result.wallTimeMs / 1000).toFixed(1);
        console.log(
          `  ${status} tests=${result.testsPassed ? "pass" : "fail"} tokens=${tokens} time=${time}s tools=${result.toolCalls}`,
        );
      }
    }
  }

  // Generate and print report
  const report = generateReport(results, {
    model: config.model,
    runs: config.runs,
    exercises,
  });

  printReport(report);

  // Save results
  const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
  const filename = `benchmark-${timestamp}.json`;
  const filepath = `${config.resultsPath}/${filename}`;
  await Bun.write(filepath, JSON.stringify(report, null, 2));
  console.log(`\nResults saved to: ${filepath}`);

  // Restore global configs
  globalConfigBackup.restore();
  if (globalConfigBackup.backedUp.length > 0) {
    console.log(`Restored ${globalConfigBackup.backedUp.length} global config(s)`);
  }
}

/** All global config paths opencode reads (merges top-down). */
const GLOBAL_CONFIG_PATHS = [
  "~/.config/opencode/config.json",
  "~/.config/opencode/opencode.json",
  "~/.config/opencode/opencode.jsonc",
  "~/.opencode/opencode.json",
  "~/.opencode/opencode.jsonc",
].map((p) => p.replace("~", homedir()));

/**
 * Backup all global opencode configs and remove them.
 * This prevents opencode from merging global plugins/settings during benchmark.
 * Returns a restore function.
 */
function backupGlobalConfigs(): { backedUp: string[]; restore: () => void } {
  const backedUp: string[] = [];

  for (const configPath of GLOBAL_CONFIG_PATHS) {
    if (existsSync(configPath)) {
      const backupPath = configPath + ".bench-backup";
      try {
        renameSync(configPath, backupPath);
        backedUp.push(configPath);
      } catch {
        // If rename fails (cross-device), try copy+delete
        try {
          copyFileSync(configPath, backupPath);
          unlinkSync(configPath);
          backedUp.push(configPath);
        } catch {}
      }
    }
  }

  return {
    backedUp,
    restore: () => {
      for (const configPath of backedUp) {
        const backupPath = configPath + ".bench-backup";
        try {
          renameSync(backupPath, configPath);
        } catch {
          try {
            copyFileSync(backupPath, configPath);
            unlinkSync(backupPath);
          } catch {}
        }
      }
    },
  };
}

main().catch((e) => {
  console.error("Benchmark failed:", e);
  process.exit(1);
});
