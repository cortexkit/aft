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
import { confirm, log, note } from "../lib/prompts.js";
import { type FeatureRow, promptFeatureList } from "./feature-list.js";

/**
 * The feature wizard: a thin renderer of the binary's setup plan.
 *
 * Every row, group, label, explanation, default and proposed value comes from
 * the plan. This module only decides how to show them and turns the user's
 * choices into an answers document for `aft setup --answers`.
 */

const GITHUB_READ = "github.read";
const GITHUB_WRITE = "github.write";

/** GitHub read/write choice with the write-locks-read rule. */
export interface GithubChoice {
  /** The independent read choice, kept while write is on so it can be restored. */
  read: boolean;
  write: boolean;
  /** Whether the user changed the read choice in this session. */
  readEdited: boolean;
}

export function initialGithubChoice(plan: SetupPlan): GithubChoice {
  const read = plan.features.find((feature) => feature.id === GITHUB_READ);
  const write = plan.features.find((feature) => feature.id === GITHUB_WRITE);
  return {
    // `configured` is the saved independent read choice; `proposed` may show
    // it checked only because write implies it.
    read: read?.configured ?? false,
    write: write?.proposed ?? false,
    readEdited: false,
  };
}

/** How the read checkbox is shown: checked and locked while write is on. */
export function githubReadDisplay(choice: GithubChoice): { checked: boolean; locked: boolean } {
  return { checked: choice.write || choice.read, locked: choice.write };
}

/** Checking write locks read on; unchecking it restores the independent read choice. */
export function setGithubWrite(choice: GithubChoice, write: boolean): GithubChoice {
  return { ...choice, write };
}

/** Toggling read has no effect while write holds the lock. */
export function setGithubRead(choice: GithubChoice, read: boolean): GithubChoice {
  if (choice.write) return choice;
  return { ...choice, read, readEdited: true };
}

/** Checkbox rows: everything except the GitHub pair, which has its own flow. */
export function checkboxRows(plan: SetupPlan): PlanFeature[] {
  return plan.features.filter((feature) => feature.kind !== "capability");
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
 * Answers for a completed wizard. Every checkbox row is sent as shown, so an
 * untouched save materializes the proposed values. GitHub read is omitted
 * while write is on unless the user edited it: write implies read when the
 * config is loaded, and saving the implied value would turn it into an
 * explicit choice.
 */
export function buildAnswers(
  plan: SetupPlan,
  selections: Record<string, boolean>,
  github: GithubChoice,
): SetupAnswers {
  const answers: Record<string, boolean> = {};
  for (const feature of checkboxRows(plan)) {
    answers[feature.id] = selections[feature.id] ?? feature.proposed;
  }
  const ids = new Set(plan.features.map((feature) => feature.id));
  if (ids.has(GITHUB_WRITE)) answers[GITHUB_WRITE] = github.write;
  if (ids.has(GITHUB_READ) && (!github.write || github.readEdited)) {
    answers[GITHUB_READ] = github.read;
  }
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
  ): Promise<string[]>;
  confirm(message: string, initial: boolean): Promise<boolean>;
  info(message: string): void;
  warn?(message: string): void;
  note(message: string, title: string): void;
}

const clackIO: WizardIO = {
  selectRows: (message, options, initial) =>
    promptFeatureList(message, options, initial, () => {
      log.warn("Cancelled.");
      process.exit(0);
    }),
  confirm: (message, initial) => confirm(message, initial),
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

/** GitHub read, described by what it lets the agent do rather than the URI schemes it serves. */
const GITHUB_READ_PROMPT =
  "Let the agent read GitHub issues and pull requests? (uses the GitHub CLI, gh, signed in to your account)";

/**
 * GitHub write, asked in the same question form as read. It is asked only
 * after read is answered yes: write needs read, so offering it after a "no"
 * would either be pointless or silently turn read back on.
 */
const GITHUB_WRITE_PROMPT =
  "Also let the agent post comments on GitHub issues and pull requests? (uses the same gh account)";

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

  const initial = Object.entries(initialSelections(plan))
    .filter(([, on]) => on)
    .map(([id]) => id);
  const picked = new Set(
    await io.selectRows("Choose the AFT features to enable", featureListGroups(plan), initial),
  );
  const selections: Record<string, boolean> = {};
  for (const feature of checkboxRows(plan)) selections[feature.id] = picked.has(feature.id);

  // Read is asked first: it is the base capability, and write builds on it.
  // Declining read therefore also turns write off, since write implies read.
  let github = initialGithubChoice(plan);
  const write = plan.features.find((feature) => feature.id === GITHUB_WRITE);
  const read = plan.features.find((feature) => feature.id === GITHUB_READ);
  let wantRead = githubReadDisplay(github).checked;
  if (read) wantRead = await io.confirm(GITHUB_READ_PROMPT, wantRead);
  if (!wantRead) {
    github = setGithubRead(setGithubWrite(github, false), false);
  } else {
    if (write) {
      github = setGithubWrite(github, await io.confirm(GITHUB_WRITE_PROMPT, github.write));
    }
    // With write off, read stands on its own and must be saved as chosen.
    if (!github.write) github = setGithubRead(github, true);
    const warning = ghWarning(checkGh());
    if (warning) (io.warn ?? io.info)(warning);
  }
  return buildAnswers(plan, selections, github);
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
