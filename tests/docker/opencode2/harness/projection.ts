import { fail } from "./errors.js";
import type {
  ProjectionRule,
  ProjectionTypeMap,
  ScenarioComparison,
  ScenarioDefinition,
  ToolCallPlan,
} from "./types.js";
import { asRecord } from "./util.js";

export const TRUNCATION_TRAILER_PATTERN =
  "^shown (?<shown>\\d+) of (?:≥)?(?<total>\\d+) (?<unit>[^ ]+) \\((?<reason>cap|depth|budget|walk)\\) · narrow: (?<narrow>.+)$";

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

export function assertComparison(actual: string, comparison: ScenarioComparison): void {
  if (comparison.mode === "exact") {
    if (actual !== comparison.expected) {
      throw new Error(
        `exact comparison failed\nexpected: ${JSON.stringify(comparison.expected)}\nactual: ${JSON.stringify(actual)}`,
      );
    }
    return;
  }
  const actualShape = projectText(actual, comparison.rules);
  if (JSON.stringify(actualShape) !== JSON.stringify(comparison.expected)) {
    throw new Error(
      `shape comparison failed\nexpected: ${JSON.stringify(comparison.expected)}\nactual: ${JSON.stringify(actualShape)}`,
    );
  }
}
