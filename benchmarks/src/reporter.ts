import type {
  RunResult,
  BenchmarkReport,
  AggregateStats,
  ComparisonStats,
  BenchMode,
} from "./config";

function failedToolCallsStr(results: RunResult[], mode: BenchMode): string {
  const modeResults = results.filter((r) => r.mode === mode);
  const totalFailed = modeResults.reduce((sum, r) => sum + (r.failedToolCalls ?? 0), 0);
  const totalCalls = modeResults.reduce((sum, r) => sum + r.toolCalls, 0);
  if (totalCalls === 0) return "0";
  return `${totalFailed}/${totalCalls} (${((totalFailed / totalCalls) * 100).toFixed(1)}%)`;
}
function median(values: number[]): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 !== 0
    ? sorted[mid]
    : (sorted[mid - 1] + sorted[mid]) / 2;
}

function mean(values: number[]): number {
  if (values.length === 0) return 0;
  return values.reduce((a, b) => a + b, 0) / values.length;
}

function aggregateStats(results: RunResult[]): AggregateStats {
  const successful = results.filter((r) => r.success);
  const tokens = results.map((r) => r.tokens.total);
  const wallTimes = results.map((r) => r.wallTimeMs);
  const toolCallCounts = results.map((r) => r.toolCalls);

  const distribution: Record<string, number> = {};
  for (const r of results) {
    for (const [name, count] of Object.entries(r.toolCallsByName)) {
      distribution[name] = (distribution[name] ?? 0) + count;
    }
  }

  return {
    totalRuns: results.length,
    successRate:
      results.length > 0 ? successful.length / results.length : 0,
    avgTokens: mean(tokens),
    medianTokens: median(tokens),
    avgWallTime: mean(wallTimes),
    medianWallTime: median(wallTimes),
    avgToolCalls: mean(toolCallCounts),
    toolCallDistribution: distribution,
  };
}

function compareStats(
  withAft: AggregateStats,
  withoutAft: AggregateStats,
): ComparisonStats {
  const tokenSavings =
    withoutAft.avgTokens > 0
      ? ((withoutAft.avgTokens - withAft.avgTokens) / withoutAft.avgTokens) *
        100
      : 0;

  const timeSavings =
    withoutAft.avgWallTime > 0
      ? ((withoutAft.avgWallTime - withAft.avgWallTime) /
          withoutAft.avgWallTime) *
        100
      : 0;

  return {
    tokenSavingsPercent: Math.round(tokenSavings * 10) / 10,
    timeSavingsPercent: Math.round(timeSavings * 10) / 10,
    toolCallReduction: Math.round(
      (withoutAft.avgToolCalls - withAft.avgToolCalls) * 10,
    ) / 10,
    successRateDelta: Math.round(
      (withAft.successRate - withoutAft.successRate) * 1000,
    ) / 10,
  };
}

export function generateReport(
  results: RunResult[],
  config: { model: string; runs: number; exercises: string[] },
): BenchmarkReport {
  const withAftResults = results.filter((r) => r.mode === "with-aft");
  const withoutAftResults = results.filter((r) => r.mode === "without-aft");

  const withAft = aggregateStats(withAftResults);
  const withoutAft = aggregateStats(withoutAftResults);
  const comparison = compareStats(withAft, withoutAft);

  return {
    config: { ...config, timestamp: new Date().toISOString() },
    results,
    summary: { withAft, withoutAft, comparison },
  };
}

export function printReport(report: BenchmarkReport): void {
  const { summary, config } = report;
  const { withAft, withoutAft, comparison } = summary;

  console.log("\n" + "=".repeat(70));
  console.log("  AFT BENCHMARK RESULTS");
  console.log("=".repeat(70));
  console.log(`  Model: ${config.model}`);
  console.log(`  Exercises: ${config.exercises.length}`);
  console.log(`  Runs per config: ${config.runs}`);
  console.log(`  Timestamp: ${config.timestamp}`);
  console.log("=".repeat(70));

  console.log("\n  SUMMARY");
  console.log("-".repeat(70));
  console.log(
    `  ${"Metric".padEnd(25)} ${"Without AFT".padEnd(18)} ${"With AFT".padEnd(18)} Delta`,
  );
  console.log("-".repeat(70));

  const rows = [
    [
      "Success Rate",
      `${(withoutAft.successRate * 100).toFixed(1)}%`,
      `${(withAft.successRate * 100).toFixed(1)}%`,
      `${comparison.successRateDelta > 0 ? "+" : ""}${comparison.successRateDelta}%`,
    ],
    [
      "Avg Tokens",
      withoutAft.avgTokens.toLocaleString(),
      withAft.avgTokens.toLocaleString(),
      `${comparison.tokenSavingsPercent > 0 ? "-" : "+"}${Math.abs(comparison.tokenSavingsPercent)}%`,
    ],
    [
      "Median Tokens",
      withoutAft.medianTokens.toLocaleString(),
      withAft.medianTokens.toLocaleString(),
      "",
    ],
    [
      "Avg Wall Time",
      `${(withoutAft.avgWallTime / 1000).toFixed(1)}s`,
      `${(withAft.avgWallTime / 1000).toFixed(1)}s`,
      `${comparison.timeSavingsPercent > 0 ? "-" : "+"}${Math.abs(comparison.timeSavingsPercent)}%`,
    ],
    [
      "Avg Tool Calls",
      withoutAft.avgToolCalls.toFixed(1),
      withAft.avgToolCalls.toFixed(1),
      `${comparison.toolCallReduction > 0 ? "-" : "+"}${Math.abs(comparison.toolCallReduction)}`,
    ],
    [
      "Failed Tool Calls",
      failedToolCallsStr(report.results, "without-aft"),
      failedToolCallsStr(report.results, "with-aft"),
      "",
    ],
  ];

  for (const [metric, without, withA, delta] of rows) {
    console.log(
      `  ${metric.padEnd(25)} ${without.padEnd(18)} ${withA.padEnd(18)} ${delta}`,
    );
  }

  console.log("\n  TOOL CALL DISTRIBUTION");
  console.log("-".repeat(70));

  const allTools = new Set([
    ...Object.keys(withoutAft.toolCallDistribution),
    ...Object.keys(withAft.toolCallDistribution),
  ]);

  for (const tool of [...allTools].sort()) {
    const without = withoutAft.toolCallDistribution[tool] ?? 0;
    const withA = withAft.toolCallDistribution[tool] ?? 0;
    console.log(
      `  ${tool.padEnd(30)} ${String(without).padEnd(18)} ${withA}`,
    );
  }

  // Per-exercise breakdown
  console.log("\n  PER-EXERCISE RESULTS");
  console.log("-".repeat(70));
  console.log(
    `  ${"Exercise".padEnd(25)} ${"W/O AFT".padEnd(12)} ${"W/ AFT".padEnd(12)} ${"Tokens W/O".padEnd(14)} Tokens W/`,
  );
  console.log("-".repeat(70));

  const exercises = [...new Set(report.results.map((r) => r.exercise))].sort();
  for (const ex of exercises) {
    const without = report.results.filter(
      (r) => r.exercise === ex && r.mode === "without-aft",
    );
    const withA = report.results.filter(
      (r) => r.exercise === ex && r.mode === "with-aft",
    );

    const wPass = without.filter((r) => r.success).length;
    const aPass = withA.filter((r) => r.success).length;
    const wTokens = mean(without.map((r) => r.tokens.total));
    const aTokens = mean(withA.map((r) => r.tokens.total));

    console.log(
      `  ${ex.padEnd(25)} ${`${wPass}/${without.length}`.padEnd(12)} ${`${aPass}/${withA.length}`.padEnd(12)} ${Math.round(wTokens).toLocaleString().padEnd(14)} ${Math.round(aTokens).toLocaleString()}`,
    );
  }

  console.log("\n" + "=".repeat(70));
}
