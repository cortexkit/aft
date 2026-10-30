import { resolveCortexKitUserConfigPath, spawnSync } from "@cortexkit/aft-bridge";
import { CLI } from "../lib/cli.js";
import {
  explicitHarness,
  loadFeaturePlan,
  type NativeRunner,
  type PlanFeature,
  runNative,
  SETUP_PLAN_VERSION,
  type SetupAnswers,
  type SetupPlan,
  withCliCommands,
  writeFeatureAnswers,
} from "../lib/feature-plan.js";
import { formatFsError, isPermissionError, tildePath } from "../lib/fs-errors.js";
import { log, note } from "../lib/prompts.js";
import {
  type FeatureRow,
  normalizeSelection,
  promptFeatureList,
  type SelectionRules,
} from "./feature-list.js";

/**
 * The feature wizard: a thin renderer of the binary's setup plan.
 *
 * Every row, group, label, explanation, default and proposed value comes from
 * the plan. This module only decides how to show them and turns the user's
 * choices into an answers document for `aft setup --answers`.
 */

const GITHUB_READ = "github.read";

/**
 * How rows depend on each other, read from the plan's kinds and
 * prerequisites. A setting row (bash compression, rewrites, background
 * commands) requires the tool row it configures and does not apply while that
 * tool is unchecked. A capability row that lists another capability as a
 * prerequisite needs it checked: GitHub write needs GitHub read. A tool's
 * prerequisites on indexes describe runtime dependencies only and never
 * change the checklist.
 */
export function selectionRules(plan: SetupPlan): SelectionRules {
  const byId = new Map(plan.features.map((feature) => [feature.id, feature]));
  const requires = new Map<string, string>();
  const implies = new Map<string, string>();
  const labels = new Map<string, string>();
  for (const feature of plan.features) {
    for (const id of feature.prerequisites) {
      const prerequisite = byId.get(id);
      if (!prerequisite) continue;
      if (feature.kind === "setting" && prerequisite.kind === "tool") {
        requires.set(feature.id, id);
        labels.set(id, prerequisite.label);
      } else if (feature.kind === "capability" && prerequisite.kind === "capability") {
        implies.set(feature.id, id);
      }
    }
  }
  return { requires, implies, labels };
}

/** Checkbox rows: every row of the plan. */
export function checkboxRows(plan: SetupPlan): PlanFeature[] {
  return plan.features;
}

/** Initial checkbox values: the plan's proposed value for each row. */
export function initialSelections(plan: SetupPlan): Record<string, boolean> {
  const selections: Record<string, boolean> = {};
  for (const feature of checkboxRows(plan)) selections[feature.id] = feature.proposed;
  return selections;
}

/** Rows grouped by the plan's `group`, in plan order. */
export function groupRows(plan: SetupPlan): Map<string, PlanFeature[]> {
  const groups = new Map<string, PlanFeature[]>();
  for (const feature of [...checkboxRows(plan)].sort((a, b) => a.order - b.order)) {
    const rows = groups.get(feature.group) ?? [];
    rows.push(feature);
    groups.set(feature.group, rows);
  }
  return groups;
}

/**
 * Answers for a completed wizard. Every row is sent as shown, so an untouched
 * save writes the proposed values explicitly, including a setting whose tool
 * is unchecked (its remembered value) and GitHub read next to GitHub write.
 * A checked row whose prerequisite is unchecked gets the prerequisite too.
 */
export function buildAnswers(plan: SetupPlan, selections: Record<string, boolean>): SetupAnswers {
  const picked = checkboxRows(plan)
    .filter((feature) => selections[feature.id] ?? feature.proposed)
    .map((feature) => feature.id);
  const checked = new Set(normalizeSelection(picked, selectionRules(plan)));
  const answers: Record<string, boolean> = {};
  for (const feature of checkboxRows(plan)) answers[feature.id] = checked.has(feature.id);
  return { plan_version: SETUP_PLAN_VERSION, selections: answers };
}

/**
 * Explanations shown before the checkboxes: every row's cost note, the move
 * and delete safety notes, and why a proposed value differs from the saved one.
 */
export function explanationLines(plan: SetupPlan): string[] {
  const lines: string[] = [];
  for (const feature of plan.features) {
    if (feature.id === "aft_move" || feature.id === "aft_delete") {
      lines.push(`${feature.label}: ${feature.description}`);
    }
    if (feature.cost_note) lines.push(`${feature.label}: ${feature.cost_note}`);
    if (feature.proposed !== feature.configured && feature.kind !== "capability") {
      const cause = feature.unavailable_reason ? ` (${feature.unavailable_reason})` : "";
      lines.push(
        `${feature.label}: proposed ${feature.proposed ? "on" : "off"} for this machine${cause}; saving records that choice, the default itself is unchanged.`,
      );
    }
  }
  return lines;
}

/** Prompt operations, injectable so tests can drive the wizard. */
export interface WizardIO {
  selectRows(
    message: string,
    options: Record<string, FeatureRow[]>,
    initial: string[],
    rules: SelectionRules,
  ): Promise<string[]>;
  info(message: string): void;
  warn?(message: string): void;
  note(message: string, title: string): void;
}

const clackIO: WizardIO = {
  selectRows: (message, options, initial, rules) =>
    promptFeatureList(
      message,
      options,
      initial,
      () => {
        log.warn("Cancelled.");
        process.exit(130);
      },
      {},
      rules,
    ),
  info: (message) => log.info(message),
  warn: (message) => log.warn(message),
  note: (message, title) => note(message, title),
};

/**
 * The checklist rows: each feature's name and its description, nothing else.
 * Runtime state ("ready", "unavailable: <code>") describes the machine at this
 * moment, not the choice being made, and on a fresh install every index reads
 * as unavailable; doctor reports it instead.
 */
export function featureListGroups(plan: SetupPlan): Record<string, FeatureRow[]> {
  const groups: Record<string, FeatureRow[]> = {};
  for (const [group, rows] of groupRows(plan)) {
    groups[group] = rows.map((feature) => ({
      value: feature.id,
      label: feature.label,
      description: feature.description.trim() || feature.label,
    }));
  }
  return groups;
}

export type GhStatus = "ready" | "missing" | "signed_out";

/** Whether `gh` is on PATH and signed in. Called at most once per wizard run. */
export function checkGhStatus(): GhStatus {
  const version = spawnSync("gh", ["--version"], { stdio: "ignore", timeout: 5_000 });
  if (version.error || version.status !== 0) return "missing";
  const auth = spawnSync("gh", ["auth", "status"], { stdio: "ignore", timeout: 10_000 });
  return !auth.error && auth.status === 0 ? "ready" : "signed_out";
}

function ghWarning(status: GhStatus): string | null {
  if (status === "missing") {
    return "GitHub read needs the GitHub CLI (gh), which is not on PATH. Install it from https://cli.github.com and run `gh auth login`; until then the agent cannot read issues or pull requests.";
  }
  if (status === "signed_out") {
    return "GitHub read needs the GitHub CLI signed in, and `gh auth status` reports no signed-in account. Run `gh auth login`; until then the agent cannot read issues or pull requests.";
  }
  return null;
}

/** Render the plan and collect the user's choices. */
export async function runFeatureWizard(
  plan: SetupPlan,
  io: WizardIO = clackIO,
  checkGh: () => GhStatus = checkGhStatus,
): Promise<SetupAnswers> {
  const explanations = explanationLines(plan);
  if (explanations.length > 0) io.note(explanations.join("\n"), "About these features");

  const rules = selectionRules(plan);
  const initial = Object.entries(initialSelections(plan))
    .filter(([, on]) => on)
    .map(([id]) => id);
  const picked = new Set(
    normalizeSelection(
      await io.selectRows(
        "Choose the AFT features to enable",
        featureListGroups(plan),
        normalizeSelection(initial, rules),
        rules,
      ),
      rules,
    ),
  );
  const selections: Record<string, boolean> = {};
  for (const feature of checkboxRows(plan)) selections[feature.id] = picked.has(feature.id);

  // GitHub read (and write, which needs it) runs through the GitHub CLI; say
  // so now rather than at the agent's first failed read.
  if (picked.has(GITHUB_READ)) {
    const warning = ghWarning(checkGh());
    if (warning) (io.warn ?? io.info)(warning);
  }
  return buildAnswers(plan, selections);
}

export interface FeatureSetupDeps {
  run?: NativeRunner;
  io?: WizardIO;
  /** GitHub CLI check for the GitHub read warning (tests stub it). */
  checkGh?: () => GhStatus;
  interactive?: boolean;
  stdout?: (text: string) => void;
  stderr?: (text: string) => void;
}

function optionValue(argv: string[], name: string): string | null {
  const index = argv.indexOf(name);
  if (index !== -1 && index + 1 < argv.length) return argv[index + 1] ?? null;
  const inline = argv.find((arg) => arg.startsWith(`${name}=`));
  return inline ? inline.slice(name.length + 1) : null;
}

/** Whether `argv` asks for the non-interactive feature modes. */
export function featureMode(argv: string[]): "plan" | "yes" | "answers" | "interactive" {
  if (argv.includes("--plan")) return "plan";
  if (argv.includes("--yes") || argv.includes("-y")) return "yes";
  if (optionValue(argv, "--answers") !== null) return "answers";
  return "interactive";
}

/**
 * The feature step of `aft setup` and `aft doctor --reconfigure`. `--plan`,
 * `--yes` and `--answers` pass straight through to the binary; the default
 * mode renders the plan interactively.
 */
export async function runFeatureSetup(
  argv: string[],
  deps: FeatureSetupDeps = {},
): Promise<number> {
  const run = deps.run ?? runNative;
  const stdout = deps.stdout ?? ((text: string) => process.stdout.write(text));
  const stderr = deps.stderr ?? ((text: string) => process.stderr.write(text));
  const harness = explicitHarness(argv);
  const harnessArgs = harness ? ["--harness", harness] : [];
  const yes = argv.includes("--yes") || argv.includes("-y");
  const answersPath = optionValue(argv, "--answers");
  if (yes && answersPath !== null) {
    stderr("--yes and --answers are mutually exclusive\n");
    return 2;
  }

  const mode = featureMode(argv);
  if (mode !== "interactive") {
    const args =
      mode === "plan"
        ? ["setup", "--plan", ...harnessArgs]
        : mode === "yes"
          ? ["setup", "--yes", ...harnessArgs]
          : ["setup", "--answers", answersPath ?? "-", ...harnessArgs];
    const result = run(args);
    if (result.stdout && mode === "plan") stdout(result.stdout);
    if (result.stderr) stderr(result.stderr.endsWith("\n") ? result.stderr : `${result.stderr}\n`);
    return result.ok ? 0 : (result.status ?? 1);
  }

  if (!featureWizardIsInteractive(deps)) {
    log.info(
      `Feature choices unchanged: rerun \`${CLI} setup\` in a terminal, or pass --yes or --answers <file>.`,
    );
    return 0;
  }
  const loaded = loadFeaturePlan(harness, run);
  if (!loaded.ok) {
    log.error(loaded.error);
    return 1;
  }
  if (loaded.warnings) log.warn(withCliCommands(loaded.warnings));
  const answers = await runFeatureWizard(loaded.plan, deps.io, deps.checkGh);
  const written = writeFeatureAnswers(answers, harness, true, run);
  if (!written.ok) {
    log.error(describeNativeFailure(written.stderr) || "aft setup --answers failed");
    return written.status ?? 1;
  }
  log.success(`Saved feature choices to ${tildePath(writtenPath(written.stdout))}.`);
  return 0;
}

/** Whether the default (no-flag) feature step will prompt, so it needs the binary. */
export function featureWizardIsInteractive(deps: FeatureSetupDeps = {}): boolean {
  return deps.interactive ?? Boolean(process.stdin.isTTY);
}

/** The file the binary reports writing (`{"written": path}`), or the default user config path. */
function writtenPath(stdout: string): string {
  try {
    const parsed = JSON.parse(stdout) as { written?: unknown };
    if (typeof parsed.written === "string" && parsed.written.length > 0) return parsed.written;
  } catch {
    // An older binary printed nothing parseable; fall back to the default location.
  }
  return resolveCortexKitUserConfigPath();
}

/**
 * A native failure as one line for the setup screen: a permission error names
 * the owner and the fix, and the binary's own `aft …` suggestions become the
 * npx command the user can run.
 */
function describeNativeFailure(stderr: string): string {
  const text = stderr.trim();
  if (!text) return "";
  const permission = text.split("\n").find((line) => isPermissionError(line));
  return permission ? formatFsError(new Error(permission)) : withCliCommands(text);
}
