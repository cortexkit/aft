import { join, resolve } from "path";

export interface BenchmarkConfig {
  /** Model to use (e.g., "anthropic/claude-sonnet-4-6") */
  model: string;
  /** Number of runs per exercise per configuration */
  runs: number;
  /** Exercises to run (empty = all 25) */
  exercises: string[];
  /** Timeout per exercise in ms (default: 5 minutes) */
  timeout: number;
  /** Path to exercism-typescript exercises */
  exercisesPath: string;
  /** Path to store results */
  resultsPath: string;
  /** Verbose logging */
  verbose: boolean;
}

export const DEFAULT_CONFIG: BenchmarkConfig = {
  model: "anthropic/claude-sonnet-4-6",
  runs: 5,
  exercises: [],
  timeout: 5 * 60 * 1000,
  exercisesPath: resolve(
    import.meta.dir,
    "../../ts-bench/exercism-typescript/exercises/practice",
  ),
  resultsPath: resolve(import.meta.dir, "../results"),
  verbose: false,
};

/** All 25 default exercises (alphabetically sorted, matching ts-bench) */
export const DEFAULT_EXERCISES = [
  "accumulate",
  "acronym",
  "all-your-base",
  "allergies",
  "alphametics",
  "anagram",
  "armstrong-numbers",
  "atbash-cipher",
  "bank-account",
  "beer-song",
  "binary-search",
  "binary-search-tree",
  "bob",
  "bowling",
  "circular-buffer",
  "clock",
  "collatz-conjecture",
  "complex-numbers",
  "connect",
  "crypto-square",
  "custom-set",
  "darts",
  "diamond",
  "difference-of-squares",
  "diffie-hellman",
];

export type BenchMode = "with-aft" | "without-aft";

export interface RunResult {
  exercise: string;
  mode: BenchMode;
  runIndex: number;
  success: boolean;
  testsPassed: boolean;
  wallTimeMs: number;
  agentTimeMs: number;
  tokens: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
    reasoning: number;
    total: number;
  };
  toolCalls: number;
  failedToolCalls: number;
  failedToolCallsByName: Record<string, number>;
  toolCallsByName: Record<string, number>;
  sessionId: string | null;
  error?: string;
}

export interface BenchmarkReport {
  config: {
    model: string;
    runs: number;
    exercises: string[];
    timestamp: string;
  };
  results: RunResult[];
  summary: {
    withAft: AggregateStats;
    withoutAft: AggregateStats;
    comparison: ComparisonStats;
  };
}

export interface AggregateStats {
  totalRuns: number;
  successRate: number;
  avgTokens: number;
  medianTokens: number;
  avgWallTime: number;
  medianWallTime: number;
  avgToolCalls: number;
  toolCallDistribution: Record<string, number>;
}

export interface ComparisonStats {
  tokenSavingsPercent: number;
  timeSavingsPercent: number;
  toolCallReduction: number;
  successRateDelta: number;
}
