import { spawnSync } from "@cortexkit/aft-bridge";
import { findAftBinary } from "./binary-probe.js";
import { CLI } from "./cli.js";

/**
 * Types and invocation helpers for the feature plan owned by the native
 * binary (`aft setup --plan`). The CLI renders the plan and sends choices
 * back; it never derives feature state itself.
 */

export const SETUP_PLAN_VERSION = 1;

export type FeatureKind = "tool" | "index" | "capability";
export type FeatureEffective = "off" | "building" | "ready" | "unavailable";

export interface PlanFeature {
  id: string;
  kind: FeatureKind;
  group: string;
  order: number;
  label: string;
  description: string;
  binding: { path: string; tool_name: string | null };
  default: boolean;
  configured: boolean;
  source: "default" | "config";
  proposed: boolean;
  effective: FeatureEffective;
  reason: string | null;
  available: boolean;
  unavailable_reason: string | null;
  cost_note: string | null;
  prerequisites: string[];
}

export interface SetupPlan {
  plan_version: number;
  features: PlanFeature[];
}

export interface SetupAnswers {
  plan_version: number;
  selections: Record<string, boolean>;
}

/** Result of one native setup invocation. */
export interface NativeResult {
  ok: boolean;
  stdout: string;
  stderr: string;
  status: number | null;
  /** True when no native binary was found, so nothing ran at all. */
  missingBinary?: boolean;
}

/** Why a native command could not run when no binary is installed. */
export const MISSING_BINARY_MESSAGE = "the AFT binary is not installed";

export type NativeRunner = (args: string[], input?: string) => NativeResult;

/** Run the native binary with `args`, capturing output. */
export function runNative(args: string[], input?: string): NativeResult {
  return runNativeBinary(findAftBinary(), args, input);
}

/** A runner pinned to one binary, for a caller that just installed it. */
export function nativeRunnerFor(binary: string): NativeRunner {
  return (args, input) => runNativeBinary(binary, args, input);
}

function runNativeBinary(binary: string | null, args: string[], input?: string): NativeResult {
  if (!binary) {
    return {
      ok: false,
      stdout: "",
      stderr: `${MISSING_BINARY_MESSAGE}; run \`${CLI} doctor --fix\` to install it`,
      status: null,
      missingBinary: true,
    };
  }
  const result = spawnSync(binary, args, {
    encoding: "utf-8",
    env: process.env,
    input,
    // An older binary without these commands would start its request loop
    // instead; bound the wait so the CLI reports a failure rather than hanging.
    timeout: 60_000,
  });
  if (result.error) {
    return { ok: false, stdout: "", stderr: result.error.message, status: null };
  }
  return {
    ok: result.status === 0,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
    status: result.status,
  };
}

/** Native arguments carrying an explicit harness selector, when one was given. */
export function harnessArgs(harness: string | null | undefined): string[] {
  return harness ? ["--harness", harness] : [];
}

/** The `--harness` value the user passed, if any. Never inferred from installed hosts. */
export function explicitHarness(argv: string[]): string | null {
  const index = argv.indexOf("--harness");
  if (index !== -1 && index + 1 < argv.length) return argv[index + 1] ?? null;
  const inline = argv.find((arg) => arg.startsWith("--harness="));
  return inline ? inline.slice("--harness=".length) : null;
}

export type PlanLoad =
  | { ok: true; plan: SetupPlan; warnings: string }
  | {
      ok: false;
      error: string;
      /** The binary refused the configuration itself (it needs `aft doctor --fix`). */
      configRejected: boolean;
      /** No binary was installed, so no plan could be produced. */
      missingBinary?: boolean;
    };

/**
 * The native binary names its own commands as `aft doctor --fix` / `aft setup`,
 * but a user running this CLI through npx has no `aft` on PATH. Rewrite those
 * suggestions to the command they can actually run.
 */
export function withCliCommands(text: string): string {
  return text.replace(/(^|[^\w/@-])aft (doctor|setup)\b/g, `$1${CLI} $2`);
}

/** Load and validate the plan. Unknown plan versions are refused. */
export function loadFeaturePlan(harness: string | null, run: NativeRunner = runNative): PlanLoad {
  const result = run(["setup", "--plan", ...harnessArgs(harness)]);
  if (!result.ok) {
    const error = result.stderr.trim() || "aft setup --plan failed";
    return {
      ok: false,
      error: withCliCommands(error),
      configRejected: result.status === 1 && error.includes("aft doctor --fix"),
      missingBinary: result.missingBinary === true,
    };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(result.stdout);
  } catch (error) {
    return {
      ok: false,
      error: `aft setup --plan printed invalid JSON: ${error instanceof Error ? error.message : String(error)}`,
      configRejected: false,
    };
  }
  const plan = parsed as Partial<SetupPlan>;
  if (plan.plan_version !== SETUP_PLAN_VERSION || !Array.isArray(plan.features)) {
    return { ok: false, error: "unsupported_setup_plan_version", configRejected: false };
  }
  return { ok: true, plan: plan as SetupPlan, warnings: result.stderr.trim() };
}

/**
 * Write answers through the binary. `alreadyWarned` tells the binary this
 * setup operation already reported its load warnings (from the plan call), so
 * the write does not repeat them.
 */
export function writeFeatureAnswers(
  answers: SetupAnswers,
  harness: string | null,
  alreadyWarned: boolean,
  run: NativeRunner = runNative,
): NativeResult {
  return run(
    [
      "setup",
      "--answers",
      "-",
      ...harnessArgs(harness),
      ...(alreadyWarned ? ["--no-load-warnings"] : []),
    ],
    JSON.stringify(answers),
  );
}

/** One file's outcome from `aft fix-config`. */
export interface ConfigFixFile {
  path: string;
  tier: "user" | "project";
  status: "rewritten" | "unchanged" | "failed";
  notes: string[];
  error: string | null;
}

export type ConfigFixRun =
  | { ok: true; files: ConfigFixFile[] }
  | { ok: false; error: string; missingBinary: boolean };

/**
 * Run the configuration migration (`aft fix-config`). Per-file outcomes are
 * returned even when some files failed; `ok:false` means the run produced no
 * usable report at all.
 */
export function runConfigFix(run: NativeRunner = runNative): ConfigFixRun {
  const result = run(["fix-config"]);
  try {
    const report = JSON.parse(result.stdout) as { files?: ConfigFixFile[] };
    if (Array.isArray(report.files)) return { ok: true, files: report.files };
  } catch {
    // Fall through: an older binary without the command prints usage instead.
  }
  return {
    ok: false,
    error: result.stderr.trim() || "aft fix-config failed",
    missingBinary: result.missingBinary === true,
  };
}
