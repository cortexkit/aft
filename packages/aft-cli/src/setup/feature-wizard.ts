import { groupMultiselect, isCancel } from "@clack/prompts";
import {
  explicitHarness,
  loadFeaturePlan,
  type NativeRunner,
  type PlanFeature,
  runNative,
  SETUP_PLAN_VERSION,
  type SetupAnswers,
  type SetupPlan,
  writeFeatureAnswers,
} from "../lib/feature-plan.js";
import { confirm, log, note } from "../lib/prompts.js";

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

/** Current-state suffix for a row, straight from the plan. */
export function describeState(feature: PlanFeature): string {
  const cause = feature.unavailable_reason ? `: ${feature.unavailable_reason}` : "";
  return `now ${feature.effective}${cause}`;
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
    options: Record<string, { value: string; label: string; hint: string }[]>,
    initial: string[],
  ): Promise<string[]>;
  confirm(message: string, initial: boolean): Promise<boolean>;
  info(message: string): void;
  note(message: string, title: string): void;
}

const clackIO: WizardIO = {
  async selectRows(message, options, initial) {
    const result = await groupMultiselect<string>({
      message,
      options,
      initialValues: initial,
      required: false,
    });
    if (isCancel(result)) {
      log.warn("Cancelled.");
      process.exit(0);
    }
    return result as string[];
  },
  confirm: (message, initial) => confirm(message, initial),
  info: (message) => log.info(message),
  note: (message, title) => note(message, title),
};

/** Render the plan and collect the user's choices. */
export async function runFeatureWizard(
  plan: SetupPlan,
  io: WizardIO = clackIO,
): Promise<SetupAnswers> {
  const explanations = explanationLines(plan);
  if (explanations.length > 0) io.note(explanations.join("\n"), "About these features");

  const options: Record<string, { value: string; label: string; hint: string }[]> = {};
  for (const [group, rows] of groupRows(plan)) {
    options[group] = rows.map((feature) => ({
      value: feature.id,
      label: feature.label,
      hint: `${feature.description} (${describeState(feature)})`,
    }));
  }
  const initial = Object.entries(initialSelections(plan))
    .filter(([, on]) => on)
    .map(([id]) => id);
  const picked = new Set(
    await io.selectRows("Choose the AFT features to enable", options, initial),
  );
  const selections: Record<string, boolean> = {};
  for (const feature of checkboxRows(plan)) selections[feature.id] = picked.has(feature.id);

  let github = initialGithubChoice(plan);
  const write = plan.features.find((feature) => feature.id === GITHUB_WRITE);
  const read = plan.features.find((feature) => feature.id === GITHUB_READ);
  if (write) {
    github = setGithubWrite(
      github,
      await io.confirm(`${write.label}: ${write.description}`, github.write),
    );
  }
  if (read) {
    const display = githubReadDisplay(github);
    if (display.locked) {
      io.info(`${read.label}: on (locked while ${write?.label ?? GITHUB_WRITE} is on)`);
    } else {
      github = setGithubRead(
        github,
        await io.confirm(`${read.label}: ${read.description}`, display.checked),
      );
    }
  }
  return buildAnswers(plan, selections, github);
}

export interface FeatureSetupDeps {
  run?: NativeRunner;
  io?: WizardIO;
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

  if (!(deps.interactive ?? Boolean(process.stdin.isTTY))) {
    log.info("Feature choices unchanged: rerun in a terminal, or pass --yes or --answers <file>.");
    return 0;
  }
  const loaded = loadFeaturePlan(harness, run);
  if (!loaded.ok) {
    log.error(loaded.error);
    return 1;
  }
  if (loaded.warnings) log.warn(loaded.warnings);
  const answers = await runFeatureWizard(loaded.plan, deps.io);
  const written = writeFeatureAnswers(answers, harness, true, run);
  if (!written.ok) {
    log.error(written.stderr.trim() || "aft setup --answers failed");
    return written.status ?? 1;
  }
  log.success("Saved feature choices to the user AFT config.");
  return 0;
}

/** Doctor lines for every plan row, straight from the plan. */
export function renderFeatureStatus(plan: SetupPlan): string[] {
  return plan.features.map((feature) => {
    const parts = [
      `configured ${feature.configured ? "on" : "off"} (${feature.source})`,
      `reason: ${feature.reason ?? "none"}`,
    ];
    if (feature.unavailable_reason) parts.push(`unavailable: ${feature.unavailable_reason}`);
    return `${feature.id}: ${feature.effective} — ${parts.join("; ")}`;
  });
}
