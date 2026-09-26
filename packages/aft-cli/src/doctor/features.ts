import { CLI } from "../lib/cli.js";
import type { PlanFeature, SetupPlan } from "../lib/feature-plan.js";
import type { GhStatus } from "../setup/feature-wizard.js";

/**
 * The Features section of `aft doctor`.
 *
 * Doctor is a standalone command: it reads the configuration through the
 * binary's setup plan, but it cannot see a running AFT session. So it reports
 * only what it can know (whether each feature is turned on, and whether that
 * came from the user's config rather than the default) and leaves live index
 * state to the sidebar and `/aft-status`. The plan's `runtime_not_observed`
 * marker means exactly "nothing was observed", so it is never shown as a
 * feature being unavailable.
 *
 * Unremarkable rows are compressed to one line per group; anything that needs
 * the user's attention is returned separately so the caller can print it as a
 * warning and count it.
 */

/** Plan marker for an enabled index that a standalone command could not observe. */
const RUNTIME_NOT_OBSERVED = "runtime_not_observed";

/** Whether AFT may run git, as the binary's status reports it. */
export interface GitState {
  available: boolean;
  /** Machine-readable cause, e.g. `macos_developer_tools_missing`. */
  reason?: string;
  /** The explanation and remedy the sidebar shows for the same state. */
  message?: string;
}

/**
 * What AFT does with git, in user terms. Each keeps working in some form
 * without git (the engine falls back the way it does outside a repository),
 * so the list names what the user loses rather than what breaks.
 */
export const GIT_BACKED_FEATURES = [
  "aft_conflicts (lists merge conflicts)",
  "symbol diffs between commits",
  "incremental index updates from git history",
  "sharing one index between git worktrees",
  "detecting the GitHub repository for GitHub read/write",
];

export interface FeatureStatusFinding {
  /** One line naming the problem. */
  text: string;
  /** What to do about it, when there is something to do. */
  remedy?: string;
}

export interface FeatureStatusReport {
  /** One compressed line per plan group. */
  lines: string[];
  /** Features that are turned on but cannot work here; doctor counts these as problems. */
  problems: FeatureStatusFinding[];
  /** Machine facts that switch features off without being a fault (a Mac without developer tools). */
  notes: FeatureStatusFinding[];
}

export interface FeatureStatusContext {
  /** GitHub CLI state; asked only when GitHub read or write is on. */
  checkGh?: () => GhStatus;
  /** Git state from the binary's status, or null when it could not be asked. */
  git?: GitState | null;
}

/** On for doctor's purposes: configured on, or implied on (GitHub read under write). */
function isOn(feature: PlanFeature): boolean {
  return feature.effective !== "off";
}

/** A feature name as shown in a group line. */
function shortName(feature: PlanFeature): string {
  if (feature.kind === "index") return feature.id.replace(/^indexes\./, "");
  if (feature.kind === "capability") return feature.id.replace(/^github\./, "");
  return feature.id;
}

/**
 * Where a feature's value came from, when it is worth saying. The default is
 * never named; a config-set value is named only when it differs from the
 * default, since a config that repeats the default changes nothing.
 */
function sourceNote(feature: PlanFeature): string {
  if (feature.reason?.startsWith("implied")) return ` (${feature.reason})`;
  if (feature.source === "config" && feature.configured !== feature.default) return " (config)";
  return "";
}

function groupLine(group: string, rows: PlanFeature[]): string {
  // Small groups whose members differ in kind of meaning (GitHub read and
  // write) read better spelled out than counted.
  if (rows.every((row) => row.kind === "capability")) {
    return `${group}: ${rows.map((row) => `${shortName(row)} ${isOn(row) ? "on" : "off"}${sourceNote(row)}`).join(", ")}`;
  }
  const on = rows.filter(isOn);
  const off = rows.filter((row) => !isOn(row));
  const turnedOn = on.filter((row) => sourceNote(row) !== "");
  const named = (list: PlanFeature[]) =>
    list.map((row) => `${shortName(row)}${sourceNote(row)}`).join(", ");
  let line =
    off.length === 0
      ? `${group}: all ${rows.length} on`
      : on.length === 0
        ? `${group}: all ${rows.length} off`
        : `${group}: ${on.length} of ${rows.length} on; off: ${named(off)}`;
  if (on.length === 0 && off.some((row) => sourceNote(row) !== "")) line += ` (${named(off)})`;
  if (turnedOn.length > 0 && off.length > 0) line += `; on: ${named(turnedOn)}`;
  else if (turnedOn.length > 0) line += ` (${named(turnedOn)})`;
  return line;
}

function ghProblem(status: GhStatus, enabled: string): FeatureStatusFinding | null {
  if (status === "missing") {
    return {
      text: `${enabled}: unavailable — the GitHub CLI (gh) is not on PATH`,
      remedy: "Install gh from https://cli.github.com, then run `gh auth login`.",
    };
  }
  if (status === "signed_out") {
    return {
      text: `${enabled}: unavailable — the GitHub CLI (gh) is not signed in`,
      remedy: "Run `gh auth login`.",
    };
  }
  return null;
}

/** Build the Features section from the plan and the machine facts doctor can check. */
export function renderFeatureStatus(
  plan: SetupPlan,
  context: FeatureStatusContext = {},
): FeatureStatusReport {
  const groups = new Map<string, PlanFeature[]>();
  for (const feature of [...plan.features].sort((a, b) => a.order - b.order)) {
    const rows = groups.get(feature.group) ?? [];
    rows.push(feature);
    groups.set(feature.group, rows);
  }

  const lines: string[] = [];
  for (const [group, rows] of groups) lines.push(groupLine(group, rows));

  const problems: FeatureStatusFinding[] = [];
  for (const feature of plan.features) {
    // A real cause (an unsupported platform, an unknown tool name) is a
    // problem; the not-observed marker only says doctor cannot see a session.
    if (
      isOn(feature) &&
      feature.unavailable_reason &&
      feature.unavailable_reason !== RUNTIME_NOT_OBSERVED
    ) {
      problems.push({ text: `${feature.id}: unavailable — ${feature.unavailable_reason}` });
    }
  }

  const github = plan.features.filter((feature) => feature.kind === "capability" && isOn(feature));
  if (github.length > 0 && context.checkGh) {
    const problem = ghProblem(context.checkGh(), github.map((feature) => feature.id).join(" and "));
    if (problem) problems.push(problem);
  }

  if (plan.features.some((feature) => feature.kind === "index" && isOn(feature))) {
    lines.push(
      "Index build state is only visible inside a running session (the AFT sidebar or /aft-status).",
    );
  }

  const notes: FeatureStatusFinding[] = [];
  if (context.git && !context.git.available) {
    notes.push({
      text:
        context.git.reason === "macos_developer_tools_missing"
          ? "git: off — macOS developer tools are not installed, and running /usr/bin/git would open Apple's install dialog"
          : `git: off${context.git.reason ? ` — ${context.git.reason}` : ""}`,
      remedy: `Affects: ${GIT_BACKED_FEATURES.join("; ")}. ${
        context.git.reason === "macos_developer_tools_missing"
          ? "Install the tools with `xcode-select --install`, or put another git (for example Homebrew's) on PATH, then restart AFT."
          : (context.git.message ?? "")
      }`.trim(),
    });
  }

  lines.push(`Every feature's full state as JSON: \`${CLI} setup --plan\``);
  return { lines, problems, notes };
}
