import { fail } from "./errors.js";
import type { ProjectionRule, ProjectionTypeMap, ScenarioComparison } from "./types.js";

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
