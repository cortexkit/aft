import { fail } from "./errors.js";
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
 * has entries with nothing to suggest: `crates/aft/src/list_surfaces.rs` gives
 * the bash output surface `narrow: &[]`, and a trailer for it correctly ends
 * at the reason. Requiring the clause made that surface's trailer unmatchable,
 * so a row watching it saw no trailer at all rather than the one it had. Every
 * surface that does declare knobs still has to print them: the T6 validator
 * compares the clause against the registry, and an incomplete fixture still
 * pins its whole trailer line character for character.
 */
export const TRUNCATION_TRAILER_PATTERN =
  "^shown (?<shown>\\d+) of (?:≥)?(?<total>\\d+) (?<unit>[^ ]+) \\((?<reason>cap|depth|budget|walk)\\)(?: · narrow: (?<narrow>.+))?$";

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
  const pattern = rule.kind === "trailer" ? TRUNCATION_TRAILER_PATTERN : rule.pattern;
  const minLines = rule.kind === "trailer" ? 1 : (rule.min_lines ?? 1);
  const maxLines = rule.kind === "trailer" ? 1 : (rule.max_lines ?? minLines);
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
        const field = rule.kind === "trailer" ? (rule.field ?? "trailer") : rule.field;
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
