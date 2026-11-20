import { fail } from "./errors.js";
import {
  expectedIgnoredEntriesDisclosure,
  IGNORED_ENTRIES_DISCLOSURE_PREFIX,
  IGNORED_ENTRIES_SURFACE_ID,
} from "./ignored-entries-t6.js";
import type {
  ProjectionRule,
  ProjectionTypeMap,
  ScenarioComparison,
  ScenarioDefinition,
  ToolCallPlan,
} from "./types.js";
import { asRecord } from "./util.js";

/**
 * The one shape a truncation trailer is allowed to take.
 *
 * The `narrow:` clause is optional because the product's own surface registry
 * has entries with nothing to suggest: `crates/aft-base/src/list_surfaces.rs` gives
 * the bash output surface `narrow: &[]`, and a trailer for it correctly ends
 * at the reason. Requiring the clause made that surface's trailer unmatchable,
 * so a row watching it saw no trailer at all rather than the one it had. Every
 * surface that does declare knobs still has to print them: the T6 validator
 * compares the clause against the registry, and an incomplete fixture still
 * pins its whole trailer line character for character.
 */
export const TRUNCATION_TRAILER_PATTERN =
  "^shown (?<shown>\\d+) of (?:≥)?(?<total>\\d+) (?<unit>[^ ]+) \\((?<reason>cap|depth|budget|walk|kind)\\)(?: · narrow: (?<narrow>.+))?$";

/**
 * The kill-deadline line, in each wording the product emits
 * (`kill_deadline_sentence` in crates/aft/src/commands/bash_orchestrate.rs and
 * `taskKillDeadlineText` in packages/aft-bridge/src/bash-hints.ts):
 *
 * - default limit, primary session: `AFT kills this task at 2026-09-10
 *   10:30:00Z, when it has run 30 minutes (its default background limit)
 *   unless you pass a longer \`timeout\`; about 12 minutes remain.`
 * - default limit, delegated worker: the same deadline and remaining-time
 *   clauses, with `, but each wait you make on it moves that kill … to set
 *   your own limit` before the remaining time.
 * - caller timeout: `AFT kills this task at 2026-09-10 10:30:00Z, when it has
 *   run 45s (the \`timeout\` you passed); about 12s remain.`
 * - no deadline: `This task has no kill deadline.`
 *
 * A wording outside these fails to match and is reported as unparsed, so a
 * change to the sentence is caught here rather than swallowed.
 */
export const KILL_DEADLINE_PATTERN =
  "^(?:AFT kills this task at (?<at>\\d{4}-\\d{2}-\\d{2} \\d{2}:\\d{2}:\\d{2}(?:\\.\\d{3})?Z), when it has run (?<limit>\\d+(?:\\.\\d+)?(?: minutes?|s)) \\((?<source>its default background limit|the `timeout` you passed)\\)(?<primary> unless you pass a longer `timeout`)?(?<worker>, but each wait you make on it moves that kill to at least the worker wait limit \\(`bash\\.worker_wait_max_ms`\\) after the wait, so it is not killed while you keep waiting; pass a `timeout` to set your own limit)?(?:; about (?<remaining>\\d+(?:\\.\\d+)?(?: minutes?|s)) remain|(?<expired>; the kill deadline has passed))\\.|(?<none>This task has no kill deadline\\.))$";

/** The typed value of a kill-deadline line. */
export interface KillDeadline {
  /** The absolute UTC deadline. Absent only when no deadline exists. */
  at?: string;
  /** The limit as printed ("30 minutes", "45s"); absent when there is none. */
  limit?: string;
  /** The approximate time remaining, or "expired" if the time is past. */
  remaining?: string;
  source: "default" | "timeout" | "none";
  /** Whether the line says a worker's waits move the default kill. */
  worker_renewal: boolean;
}

function killDeadlineValue(groups: Record<string, string | undefined>): KillDeadline {
  if (groups.none !== undefined) return { source: "none", worker_renewal: false };
  const source = groups.source === "its default background limit" ? "default" : "timeout";
  // The default-limit wording always names either the primary or the worker
  // continuation; the caller-timeout wording names neither.
  if (source === "default" && groups.primary === undefined && groups.worker === undefined) {
    throw new Error("kill deadline names the default limit without its continuation");
  }
  if (source === "timeout" && (groups.primary !== undefined || groups.worker !== undefined)) {
    throw new Error("kill deadline names a caller timeout with a default-limit continuation");
  }
  return {
    at: groups.at,
    limit: groups.limit,
    remaining: groups.remaining ?? (groups.expired === undefined ? undefined : "expired"),
    source,
    worker_renewal: groups.worker !== undefined,
  };
}

function convert(value: string, type: "boolean" | "number" | "string" | undefined): unknown {
  if (type === "number") {
    if (!/^-?(?:\d+|\d+\.\d+)$/.test(value))
      throw new Error(`expected numeric capture, got ${value}`);
    return Number(value);
  }
  if (type === "boolean") {
    if (value !== "true" && value !== "false")
      throw new Error(`expected boolean capture, got ${value}`);
    return value === "true";
  }
  return value;
}

function matchRule(
  rule: ProjectionRule,
  lines: string[],
  index: number,
): { consumed: number; value?: unknown } | undefined {
  const fixedLine = rule.kind === "trailer" || rule.kind === "kill_deadline";
  const pattern =
    rule.kind === "trailer"
      ? TRUNCATION_TRAILER_PATTERN
      : rule.kind === "kill_deadline"
        ? KILL_DEADLINE_PATTERN
        : rule.pattern;
  const minLines = fixedLine ? 1 : (rule.min_lines ?? 1);
  const maxLines = fixedLine ? 1 : (rule.max_lines ?? minLines);
  if (
    !Number.isInteger(minLines) ||
    !Number.isInteger(maxLines) ||
    minLines < 1 ||
    maxLines < minLines
  ) {
    throw new Error("projection line bounds must be positive and ordered");
  }
  const expression = new RegExp(pattern);
  for (let count = maxLines; count >= minLines; count -= 1) {
    if (index + count > lines.length) continue;
    const match = expression.exec(lines.slice(index, index + count).join("\n"));
    if (!match || match.index !== 0 || match[0].length !== match.input.length) continue;
    if (rule.kind === "ignore") return { consumed: count };
    const groups = match.groups ?? {};
    if (rule.kind === "kill_deadline") {
      const value = killDeadlineValue(groups);
      if (rule.clock === "omit") {
        delete value.at;
        delete value.remaining;
      }
      return { consumed: count, value };
    }
    const types: ProjectionTypeMap | undefined =
      rule.kind === "field" ? rule.types : { shown: "number", total: "number" };
    const projected = Object.fromEntries(
      Object.entries(groups).map(([name, value]) => [name, convert(value, types?.[name])]),
    );
    const values = Object.values(projected);
    return {
      consumed: count,
      value: values.length === 1 ? values[0] : projected,
    };
  }
  return undefined;
}

/** The field a projecting rule writes; `ignore` rules write none. */
export function projectedField(rule: Exclude<ProjectionRule, { kind: "ignore" }>): string {
  if (rule.kind === "trailer") return rule.field ?? "trailer";
  if (rule.kind === "kill_deadline") return rule.field ?? "kill_deadline";
  return rule.field;
}

export function projectText(
  text: string,
  rules: readonly ProjectionRule[],
): Record<string, unknown> {
  const lines = text.replace(/\r\n/g, "\n").replace(/\n$/, "").split("\n");
  const projected: Record<string, unknown> = {};
  let index = 0;
  while (index < lines.length) {
    let accepted = false;
    for (const rule of rules) {
      const match = matchRule(rule, lines, index);
      if (!match) continue;
      if (rule.kind !== "ignore") {
        const field = projectedField(rule);
        if (Object.hasOwn(projected, field)) throw new Error(`projection field repeated: ${field}`);
        projected[field] = match.value;
      }
      index += match.consumed;
      accepted = true;
      break;
    }
    if (!accepted) {
      fail(
        "projection_unparsed",
        lines[index],
        { line: lines[index], line_number: index + 1 },
        true,
      );
    }
  }
  return projected;
}

export function assertT6Trailer(
  scenario: ScenarioDefinition,
  call: ToolCallPlan,
  text: string,
): void {
  const t6 = asRecord(scenario.metadata?.t6);
  if (!t6) return;
  const subjectCall = t6.call_id ?? scenario.compare_call_id;
  if (subjectCall !== call.id) return;
  const matches = [...text.matchAll(new RegExp(TRUNCATION_TRAILER_PATTERN, "gm"))];
  if (t6.surface_id === IGNORED_ENTRIES_SURFACE_ID) {
    const expected = expectedIgnoredEntriesDisclosure(t6.fixture);
    if (expected === undefined || t6.expected_disclosure !== expected) {
      throw new Error(`${scenario.id}: ignored_entries must pin its approved disclosure sentence`);
    }
    if (matches.length !== 0) {
      throw new Error(`${scenario.id}: ignored_entries rendered a generic truncation trailer`);
    }
    const notes = text.split(/\r?\n/).filter((line) => line.startsWith(IGNORED_ENTRIES_DISCLOSURE_PREFIX));
    if (notes.length !== 1 || notes[0] !== expected) {
      throw new Error(`${scenario.id}: ignored_entries disclosure does not exactly equal ${JSON.stringify(expected)}`);
    }
    return;
  }
  if (t6.fixture === "complete" && matches.length !== 0) {
    throw new Error(`${scenario.id}: complete T6 fixture rendered a truncation trailer`);
  }
  if (t6.fixture === "incomplete") {
    if (matches.length !== 1) {
      throw new Error(`${scenario.id}: incomplete T6 fixture rendered ${matches.length} trailers`);
    }
    if (matches[0].groups?.reason !== t6.triggered_reason) {
      throw new Error(
        `${scenario.id}: trailer reason ${String(matches[0].groups?.reason)} does not equal ${String(
          t6.triggered_reason,
        )}`,
      );
    }
    if (t6.expected_trailer !== undefined) {
      if (typeof t6.expected_trailer !== "string" || matches[0][0] !== t6.expected_trailer) {
        throw new Error(
          `${scenario.id}: trailer ${JSON.stringify(matches[0][0])} does not exactly equal ${JSON.stringify(
            t6.expected_trailer,
          )}`,
        );
      }
    }
  }
}

/**
 * AFT appends its status bar (`[AFT E.. W.. | D.. U.. C.. | T..]`) to a tool
 * result whenever the health counts change since the previous result, after one
 * newline when the text already ends with one and after two otherwise
 * (crates/aft/src/response_finalize.rs). When a count first resolves depends on
 * background work (diagnostics, the TODO scan), so whether a given call carries
 * the bar is timing, not behaviour. Comparisons judge the tool's own output.
 */
const STATUS_BAR = /\[AFT E\S* W\S* \| ~?D\S* U\S* C\S* \| T\S*\]$/;

/**
 * The texts `text` could have been before the finalizer appended its bar: the
 * text itself when it carries no bar, else the one or two readings the
 * separator allows (`x\n` + `\n` + bar and `x` + `\n\n` + bar look the same).
 */
export function withoutTrailingStatusBar(text: string): string[] {
  const bar = STATUS_BAR.exec(text);
  if (!bar) return [text];
  const before = text.slice(0, bar.index);
  if (!before.endsWith("\n\n")) return [text];
  return [before.slice(0, -1), before.slice(0, -2)];
}

/**
 * A short line diff of two texts: the first differing line with a little
 * context on each side, each line JSON-quoted so whitespace and a missing
 * final newline are visible. Enough for a failure message to say what
 * differed without dumping both outputs whole.
 */
export function describeTextDifference(left: string, right: string, context = 2): string {
  const a = left.split("\n");
  const b = right.split("\n");
  let first = 0;
  while (first < a.length && first < b.length && a[first] === b[first]) first += 1;
  if (first === a.length && first === b.length) return "(texts are identical)";
  let lastA = a.length - 1;
  let lastB = b.length - 1;
  while (lastA >= first && lastB >= first && a[lastA] === b[lastB]) {
    lastA -= 1;
    lastB -= 1;
  }
  const out: string[] = [`first difference at line ${first + 1}`];
  for (let i = Math.max(0, first - context); i < first; i += 1) out.push(`  ${JSON.stringify(a[i])}`);
  const cap = 20;
  const removed = a.slice(first, lastA + 1);
  const added = b.slice(first, lastB + 1);
  for (const line of removed.slice(0, cap)) out.push(`- ${JSON.stringify(line)}`);
  if (removed.length > cap) out.push(`- … ${removed.length - cap} more line(s)`);
  for (const line of added.slice(0, cap)) out.push(`+ ${JSON.stringify(line)}`);
  if (added.length > cap) out.push(`+ … ${added.length - cap} more line(s)`);
  const after = Math.min(a.length, lastA + 1 + context);
  for (let i = lastA + 1; i < after; i += 1) out.push(`  ${JSON.stringify(a[i])}`);
  return out.join("\n");
}

/** A field a row may leave out of its projected cross-host comparison. */
export interface ParityFieldException {
  scenario: string;
  field: string;
}

function projectionOrUndefined(
  text: string,
  rules: readonly ProjectionRule[],
): Record<string, unknown> | undefined {
  try {
    return projectText(text, rules);
  } catch {
    return undefined;
  }
}

/**
 * T7: the same scenario on the OpenCode 1 and OpenCode 2 hosts must produce
 * the same tool output. AFT's trailing status bar is left out on both sides
 * for the reason given at `STATUS_BAR`: which host's call happens to be the
 * first to see a count change is timing, not host behaviour. Everything else
 * is compared as the row declares, exactly or by projection.
 */
export function assertDualHostParity(
  scenario: ScenarioDefinition,
  v1Text: string,
  v2Text: string,
  allowlist: readonly ParityFieldException[],
): void {
  if (!scenario.comparison) throw new Error(`${scenario.id}: T7 requires a declared comparison`);
  const allowedFields = allowlist
    .filter((entry) => entry.scenario === scenario.id)
    .map((entry) => entry.field);
  const v1Readings = withoutTrailingStatusBar(v1Text);
  const v2Readings = withoutTrailingStatusBar(v2Text);
  if (scenario.comparison.mode === "exact") {
    if (allowedFields.length > 0) {
      throw new Error(`${scenario.id}: exact parity cannot have field exceptions`);
    }
    if (!v1Readings.some((reading) => v2Readings.includes(reading))) {
      throw new Error(
        `${scenario.id}: exact V1/V2 parity mismatch (- V1, + V2)\n${describeTextDifference(
          v1Readings[0],
          v2Readings[0],
        )}`,
      );
    }
    return;
  }
  const rules = scenario.comparison.rules;
  const shapesOf = (readings: string[]): string[] =>
    readings.flatMap((reading) => {
      const shape = projectionOrUndefined(reading, rules);
      if (!shape) return [];
      for (const field of allowedFields) delete shape[field];
      return [JSON.stringify(shape)];
    });
  const v1Shapes = shapesOf(v1Readings);
  const v2Shapes = shapesOf(v2Readings);
  // When none of one host's readings (with or without the status bar)
  // projects, project its first reading again unguarded so the failure is the
  // projection's own error, which names the line the rules could not parse.
  if (v1Shapes.length === 0) projectText(v1Readings[0], rules);
  if (v2Shapes.length === 0) projectText(v2Readings[0], rules);
  if (!v1Shapes.some((shape) => v2Shapes.includes(shape))) {
    throw new Error(
      `${scenario.id}: projected V1/V2 parity mismatch\nV1 ${v1Shapes[0]}\nV2 ${v2Shapes[0]}`,
    );
  }
}

export function assertComparison(actual: string, comparison: ScenarioComparison): void {
  const readings = withoutTrailingStatusBar(actual);
  if (comparison.mode === "exact") {
    if (!readings.includes(comparison.expected as string)) {
      throw new Error(
        `exact comparison failed\nexpected: ${JSON.stringify(comparison.expected)}\nactual: ${JSON.stringify(actual)}`,
      );
    }
    return;
  }
  const shapes = readings.map((reading) => JSON.stringify(projectText(reading, comparison.rules)));
  if (!shapes.includes(JSON.stringify(comparison.expected))) {
    throw new Error(
      `shape comparison failed\nexpected: ${JSON.stringify(comparison.expected)}\nactual: ${shapes[0]}`,
    );
  }
}
