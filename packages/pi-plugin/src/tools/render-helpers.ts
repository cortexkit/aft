/**
 * Shared Pi TUI rendering helpers for AFT-backed tools.
 */

import { homedir } from "node:os";
import { type AgentToolResult, renderDiff, type Theme } from "@earendil-works/pi-coding-agent";
import { type Component, Container, Spacer, Text } from "@earendil-works/pi-tui";

export interface RenderContextLike<TArgs = unknown> {
  args: TArgs;
  lastComponent: Component | undefined;
  isError: boolean;
}

export function reuseText(last: Component | undefined): Text {
  return last instanceof Text ? last : new Text("", 0, 0);
}

export function reuseContainer(last: Component | undefined): Container {
  return last instanceof Container ? last : new Container();
}

export function shortenPath(path: string): string {
  const home = homedir();
  if (path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}

export function renderToolCall(
  toolName: string,
  summary: string | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const suffix = summary ? ` ${summary}` : "";
  text.setText(`${theme.fg("toolTitle", theme.bold(toolName))}${suffix}`);
  return text;
}

export function accentPath(theme: Theme, path: string | undefined): string {
  if (!path) return theme.fg("toolOutput", "...");
  return theme.fg("accent", shortenPath(path));
}

export function collectTextContent(result: AgentToolResult<unknown>): string {
  return result.content
    .filter((part) => part.type === "text")
    .map((part) => (part as { text?: string }).text ?? "")
    .join("\n")
    .trim();
}

export function extractStructuredPayload(result: AgentToolResult<unknown>): unknown {
  if (result.details !== undefined) return result.details;
  const text = collectTextContent(result);
  if (!text) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

export function renderErrorResult(
  result: AgentToolResult<unknown>,
  fallback: string,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const message = collectTextContent(result) || fallback;
  text.setText(`\n${theme.fg("error", message)}`);
  return text;
}

export function renderTextResult(textValue: string, context: RenderContextLike): Text {
  const text = reuseText(context.lastComponent);
  text.setText(textValue ? `\n${textValue}` : "");
  return text;
}

export function renderSections(sections: string[], context: RenderContextLike): Container {
  const container = reuseContainer(context.lastComponent);
  container.clear();
  const visibleSections = sections.filter((section) => section.trim().length > 0);
  if (visibleSections.length === 0) return container;

  container.addChild(new Spacer(1));
  visibleSections.forEach((section, index) => {
    if (index > 0) container.addChild(new Spacer(1));
    container.addChild(new Text(section, 0, 0));
  });
  return container;
}

export function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

export function asRecords(value: unknown): Record<string, unknown>[] {
  return Array.isArray(value)
    ? (value.map(asRecord).filter(Boolean) as Record<string, unknown>[])
    : [];
}

export function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

export function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

export function asBoolean(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

export function joinNonEmpty(parts: Array<string | undefined>, separator = " · "): string {
  return parts.filter((part): part is string => Boolean(part && part.length > 0)).join(separator);
}

export function indentBlock(text: string, spaces = 2): string {
  const padding = " ".repeat(spaces);
  return text
    .split("\n")
    .map((line) => `${padding}${line}`)
    .join("\n");
}

export function formatValue(value: unknown): string {
  if (Array.isArray(value)) return value.map(formatValue).join(", ");
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (value === null || value === undefined) return "—";
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

export function groupByFile<T>(
  items: T[],
  getFile: (item: T) => string | undefined,
): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  items.forEach((item) => {
    const file = getFile(item) ?? "(unknown file)";
    const current = groups.get(file) ?? [];
    current.push(item);
    groups.set(file, current);
  });
  return groups;
}

export function distinctCount(values: Array<string | undefined>): number {
  return new Set(values.filter((value): value is string => Boolean(value))).size;
}

export function severityBadge(theme: Theme, severity: string): string {
  const label = severity === "information" ? "info" : severity;
  switch (severity) {
    case "error":
      return theme.fg("error", `[${label}]`);
    case "warning":
      return theme.fg("warning", `[${label}]`);
    case "information":
      return theme.fg("accent", `[${label}]`);
    case "hint":
      return theme.fg("muted", `[${label}]`);
    default:
      return theme.fg("muted", `[${label}]`);
  }
}

export function formatLineRange(startLine?: number, endLine?: number): string | undefined {
  if (startLine === undefined) return undefined;
  if (endLine === undefined || endLine === startLine) return `${startLine}`;
  return `${startLine}-${endLine}`;
}

export function formatTimestamp(value: unknown): string | undefined {
  if (typeof value === "string" && value.length > 0) return value;
  if (typeof value !== "number" || !Number.isFinite(value)) return undefined;
  const millis = value > 1_000_000_000_000 ? value : value * 1000;
  const date = new Date(millis);
  if (Number.isNaN(date.getTime())) return String(value);
  return date.toISOString().replace("T", " ").replace(".000Z", "Z");
}

export function formatUnifiedDiffForPi(unifiedDiff: string): string {
  if (!unifiedDiff.trim()) return "";

  const entries: Array<{ prefix: "+" | "-" | " "; line: number; text: string }> = [];
  const hunkHeader = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;
  let oldLine = 1;
  let newLine = 1;

  for (const line of unifiedDiff.split("\n")) {
    if (!line) continue;
    if (line.startsWith("--- ") || line.startsWith("+++ ")) continue;
    if (line.startsWith("\\ No newline at end of file")) continue;

    const headerMatch = line.match(hunkHeader);
    if (headerMatch) {
      oldLine = Number(headerMatch[1]);
      newLine = Number(headerMatch[2]);
      continue;
    }

    if (line.startsWith("+") && !line.startsWith("+++")) {
      entries.push({ prefix: "+", line: newLine, text: line.slice(1) });
      newLine += 1;
      continue;
    }

    if (line.startsWith("-") && !line.startsWith("---")) {
      entries.push({ prefix: "-", line: oldLine, text: line.slice(1) });
      oldLine += 1;
      continue;
    }

    if (line.startsWith(" ")) {
      entries.push({ prefix: " ", line: oldLine, text: line.slice(1) });
      oldLine += 1;
      newLine += 1;
    }
  }

  if (entries.length === 0) return "";

  const width = String(entries.reduce((max, entry) => Math.max(max, entry.line), 1)).length;
  return entries
    .map((entry) => `${entry.prefix}${String(entry.line).padStart(width, " ")} ${entry.text}`)
    .join("\n");
}

export function renderUnifiedDiff(unifiedDiff: string): string {
  const piDiff = formatUnifiedDiffForPi(unifiedDiff);
  if (!piDiff) return "";
  try {
    return renderDiff(piDiff);
  } catch {
    return piDiff;
  }
}
