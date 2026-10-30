/**
 * Shared flat-text formatter for aft_callgraph responses (agent + themed TUI).
 */

import { shortenHomePath as shortenPath } from "./path-display.js";

export interface CallgraphTheme {
  fg(role: string, text: string): string;
}

export interface CallgraphFormatOptions {
  includeUnresolved?: boolean;
}

const UNRESOLVED_SUMMARY_NAME_LIMIT = 10;

export const PLAIN_CALLGRAPH_THEME: CallgraphTheme = {
  fg: (_role, text) => text,
};

function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

function asRecords(value: unknown): Record<string, unknown>[] {
  return Array.isArray(value)
    ? (value.map(asRecord).filter(Boolean) as Record<string, unknown>[])
    : [];
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function asBoolean(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

function joinNonEmpty(parts: Array<string | undefined>, separator = " · "): string {
  return parts.filter((part): part is string => Boolean(part && part.length > 0)).join(separator);
}

function treeLine(depth: number, text: string): string {
  return `${"  ".repeat(depth)}${depth === 0 ? "" : "↳ "}${text}`;
}

/** Marks edges resolved purely by callee name (may be the wrong homonym). */
function nameMatchEdgeMarker(record: Record<string, unknown>, theme: CallgraphTheme): string {
  return asString(record.resolved_by) === "name_match" ? ` ${theme.fg("warning", "~")}` : "";
}

function isUnresolvedLeaf(node: Record<string, unknown>): boolean {
  return node.resolved === false && asRecords(node.children).length === 0;
}

function unresolvedSummaryText(nodes: Record<string, unknown>[]): string {
  const distinctNames: string[] = [];
  const seen = new Set<string>();
  for (const node of nodes) {
    const name = asString(node.name) ?? "(unknown)";
    if (seen.has(name)) continue;
    seen.add(name);
    distinctNames.push(name);
  }

  const displayed = distinctNames.slice(0, UNRESOLVED_SUMMARY_NAME_LIMIT);
  const hidden = distinctNames.length - displayed.length;
  const names = hidden > 0 ? `${displayed.join(", ")}, … (+${hidden} more)` : displayed.join(", ");
  const noun = nodes.length === 1 ? "call" : "calls";
  return `+ ${nodes.length} unresolved external ${noun}: ${names}`;
}

function renderCallTreeNode(
  node: Record<string, unknown>,
  depth: number,
  lines: string[],
  theme: CallgraphTheme,
  options: CallgraphFormatOptions,
): void {
  const name = asString(node.name) ?? "(unknown)";
  const file = shortenPath(asString(node.file) ?? "(unknown file)");
  const line = asNumber(node.line);
  // `resolved:false` means the callee could not be resolved to a definition —
  // the file/line is the CALLSITE in the caller, not the callee's definition.
  // Mark it so the agent doesn't read the callsite as the definition location.
  // Only when explicitly false (resolved/legacy nodes omit the field).
  const unresolved = node.resolved === false ? ` ${theme.fg("warning", "[unresolved]")}` : "";
  const nameMatch = nameMatchEdgeMarker(node, theme);
  const location = line !== undefined ? `[${file}:${line}]` : `[${file}]`;
  lines.push(treeLine(depth, `${name} ${location}${unresolved}${nameMatch}`));

  const children = asRecords(node.children);
  if (options.includeUnresolved) {
    children.forEach((child) => {
      renderCallTreeNode(child, depth + 1, lines, theme, options);
    });
    return;
  }

  const unresolvedLeaves = children.filter(isUnresolvedLeaf);
  if (unresolvedLeaves.length === 0) {
    children.forEach((child) => {
      renderCallTreeNode(child, depth + 1, lines, theme, options);
    });
    return;
  }

  const unresolvedLeafSet = new Set(unresolvedLeaves);
  let summaryInserted = false;
  children.forEach((child) => {
    if (unresolvedLeafSet.has(child)) {
      if (!summaryInserted) {
        lines.push(
          treeLine(depth + 1, theme.fg("warning", unresolvedSummaryText(unresolvedLeaves))),
        );
        summaryInserted = true;
      }
      return;
    }
    renderCallTreeNode(child, depth + 1, lines, theme, options);
  });
}

function depthWarning(
  response: Record<string, unknown>,
  theme: CallgraphTheme,
  depthField = "depth_limited",
  truncatedField = "truncated",
): string {
  const limited = asBoolean(response[depthField]);
  const truncated = asNumber(response[truncatedField]) ?? 0;
  if (!limited && truncated === 0) return "";
  const detail = truncated > 0 ? `, ${truncated} truncated` : "";
  return theme.fg("warning", `(depth limited${detail})`);
}

function hubSummaryLine(
  response: Record<string, unknown>,
  theme: CallgraphTheme,
): string | undefined {
  const summary = asRecord(response.hub_summary);
  const message = summary ? asString(summary.message) : undefined;
  return message ? theme.fg("warning", message) : undefined;
}

function renderTracePath(
  path: Record<string, unknown>,
  index: number,
  lines: string[],
  theme: CallgraphTheme,
): void {
  lines.push(`Path ${index + 1}`);
  asRecords(path.hops).forEach((hop, hopIndex) => {
    const symbol = asString(hop.symbol) ?? "(unknown)";
    const file = shortenPath(asString(hop.file) ?? "(unknown file)");
    const line = asNumber(hop.line);
    const entry = hop.is_entry_point === true ? " [entry]" : "";
    const nameMatch = nameMatchEdgeMarker(hop, theme);
    lines.push(
      treeLine(
        hopIndex + 1,
        `${symbol}${entry} ${line !== undefined ? `[${file}:${line}]` : `[${file}]`}${nameMatch}`,
      ),
    );
  });
}

function renderCallersGroupLines(
  group: Record<string, unknown>,
  theme: CallgraphTheme,
  callbackLabel: string,
): string[] {
  const file = shortenPath(asString(group.file) ?? "(unknown file)");
  const lines = [theme.fg("accent", file)];
  const callers = asRecords(group.callers);

  const bySymbolProvenance = new Map<string, number[]>();
  for (const caller of callers) {
    const symbol = asString(caller.symbol) ?? "(unknown)";
    const via = asString(caller.via) ?? "";
    const relation = asString(caller.relation) ?? "";
    const provenanceKey =
      asString(caller.resolved_by) === "name_match"
        ? `${symbol}\0${via}\0${relation}\0name_match`
        : `${symbol}\0${via}\0${relation}\0exact`;
    const line = asNumber(caller.line);
    const bucket = bySymbolProvenance.get(provenanceKey) ?? [];
    if (line !== undefined) bucket.push(line);
    bySymbolProvenance.set(provenanceKey, bucket);
  }

  const keys = [...bySymbolProvenance.keys()].sort();
  for (const key of keys) {
    const [symbol = "(unknown)", via = "", relation = ""] = key.split("\0");
    const isNameMatch = key.endsWith("\0name_match");
    const lineNums = (bySymbolProvenance.get(key) ?? []).sort((a, b) => a - b);
    const linePart = lineNums.length > 0 ? lineNums.map(String).join(", ") : "?";
    const marker = isNameMatch ? ` ${theme.fg("warning", "~")}` : "";
    const viaPart = via ? ` via ${via}` : "";
    const reference = relation === "callback_reference" ? callbackLabel : "";
    lines.push(`  ↳ ${symbol}:${linePart}${marker}${viaPart}${reference}`);
  }

  return lines;
}

function callbackReferenceCountSuffix(record: Record<string, unknown>): string {
  const count = asNumber(record.callback_dependencies) ?? 0;
  return count > 0 ? ` (${count} by callback reference)` : "";
}

export function formatCallgraphSections(
  op: string,
  response: unknown,
  theme: CallgraphTheme = PLAIN_CALLGRAPH_THEME,
  options: CallgraphFormatOptions = {},
): string[] {
  const record = asRecord(response);
  if (!record) return [theme.fg("muted", "No navigation result.")];

  if (op === "call_tree") {
    const lines: string[] = [];
    renderCallTreeNode(record, 0, lines, theme, options);
    const warning = depthWarning(record, theme);
    if (warning) lines.push(warning);
    return lines.length > 0 ? lines : [theme.fg("muted", "No call tree available.")];
  }

  if (op === "callers") {
    const groups = asRecords(record.callers);
    const warning = depthWarning(record, theme);
    const hubSummary = hubSummaryLine(record, theme);
    const hiddenTests = asNumber(record.hidden_test_callers) ?? 0;
    const total = (asNumber(record.total_callers) ?? 0) - hiddenTests;
    const callbackNote = callbackReferenceCountSuffix(record);
    const sections = [
      joinNonEmpty([
        theme.fg("success", `${total} caller${total === 1 ? "" : "s"}${callbackNote}`),
        theme.fg("muted", `${groups.length} file group${groups.length === 1 ? "" : "s"}`),
        warning,
      ]),
    ];
    const incompleteReason = asString(record.incomplete_reason);
    if (incompleteReason) sections.unshift(theme.fg("warning", incompleteReason));
    const testFilterNote = asString(record.test_filter_note);
    if (testFilterNote) sections.unshift(theme.fg("warning", testFilterNote));
    if (hubSummary) sections.push(hubSummary);
    groups.forEach((group) => {
      sections.push(
        renderCallersGroupLines(group, theme, asString(record.callback_reference_label) ?? "").join(
          "\n",
        ),
      );
    });
    if (hiddenTests > 0)
      sections.push(
        theme.fg("muted", `${hiddenTests} callers in tests hidden — includeTests: true shows them`),
      );
    // Rust macro token trees that could not be parsed may hide more callers.
    const macroNote = asString(asRecord(record.macro_note)?.message);
    if (macroNote) sections.push(theme.fg("warning", macroNote));
    return sections;
  }

  if (op === "trace_to_symbol") {
    const path = asRecords(record.path);
    const complete = asBoolean(record.complete);
    const reason = asString(record.reason);
    if (path.length === 0) {
      const prefix =
        complete === false ? theme.fg("warning", "No complete path") : theme.fg("muted", "No path");
      return [`${prefix}${reason ? ` (${reason})` : ""}`];
    }
    const lines = [theme.fg("success", `${path.length} hop${path.length === 1 ? "" : "s"}`)];
    path.forEach((hop, index) => {
      const symbol = asString(hop.symbol) ?? "(unknown)";
      const file = shortenPath(asString(hop.file) ?? "(unknown file)");
      const line = asNumber(hop.line);
      const nameMatch = nameMatchEdgeMarker(hop, theme);
      lines.push(
        treeLine(
          index + 1,
          `${symbol} ${line !== undefined ? `[${file}:${line}]` : `[${file}]`}${nameMatch}`,
        ),
      );
    });
    return lines;
  }

  if (op === "trace_to") {
    const paths = asRecords(record.paths);
    const warning = depthWarning(record, theme, "max_depth_reached", "truncated_paths");
    const hubSummary = hubSummaryLine(record, theme);
    const totalPaths = asNumber(record.total_paths) ?? paths.length;
    const totalPathsIsLowerBound = asBoolean(record.total_paths_is_lower_bound) ?? false;
    const entryPoints = asNumber(record.entry_points_found) ?? 0;
    const sections = [
      joinNonEmpty([
        theme.fg(
          "success",
          `${totalPathsIsLowerBound ? "at least " : ""}${totalPaths} path${totalPaths === 1 ? "" : "s"}`,
        ),
        theme.fg("muted", `${entryPoints} entry point${entryPoints === 1 ? "" : "s"}`),
        warning,
      ]),
    ];
    if (hubSummary) sections.push(hubSummary);
    if (paths.length === 0) sections.push(theme.fg("muted", "No entry paths found."));
    paths.forEach((path, index) => {
      const lines: string[] = [];
      renderTracePath(path, index, lines, theme);
      sections.push(lines.join("\n"));
    });
    return sections;
  }

  if (op === "impact") {
    const callers = asRecords(record.callers);
    const warning = depthWarning(record, theme);
    const hubSummary = hubSummaryLine(record, theme);
    const hiddenTests = asNumber(record.hidden_test_callers) ?? 0;
    const totalAffected = (asNumber(record.total_affected) ?? callers.length) - hiddenTests;
    const callbackNote = callbackReferenceCountSuffix(record);
    const siteKind = callbackNote ? "site" : "call site";
    const affectedFiles = asNumber(record.affected_files) ?? 0;
    const sections = [
      joinNonEmpty([
        theme.fg(
          "warning",
          `${totalAffected} affected ${siteKind}${totalAffected === 1 ? "" : "s"}${callbackNote}`,
        ),
        theme.fg("muted", `${affectedFiles} file${affectedFiles === 1 ? "" : "s"}`),
        warning,
      ]),
    ];
    const incompleteReason = asString(record.incomplete_reason);
    if (incompleteReason) sections.unshift(theme.fg("warning", incompleteReason));
    const testFilterNote = asString(record.test_filter_note);
    if (testFilterNote) sections.unshift(theme.fg("warning", testFilterNote));
    if (hubSummary) sections.push(hubSummary);
    if (callers.length === 0) sections.push(theme.fg("muted", "No impacted callers found."));
    callers.forEach((caller) => {
      const file = shortenPath(asString(caller.caller_file) ?? "(unknown file)");
      const symbol = asString(caller.caller_symbol) ?? "(unknown)";
      const line = asNumber(caller.line) ?? 0;
      const entry = caller.is_entry_point === true ? ` ${theme.fg("warning", "[entry]")}` : "";
      const nameMatch = nameMatchEdgeMarker(caller, theme);
      const reference =
        caller.relation === "callback_reference"
          ? (asString(record.callback_reference_label) ?? "")
          : "";
      const expression = asString(caller.call_expression);
      const params = Array.isArray(caller.parameters)
        ? caller.parameters.map(String).join(", ")
        : "";
      sections.push(
        [
          `${theme.fg("accent", file)}:${line}`,
          `  ↳ ${symbol}${entry}${nameMatch}${reference}`,
          expression ? `  ${theme.fg("muted", expression)}` : undefined,
          params ? `  ${theme.fg("muted", `params: ${params}`)}` : undefined,
        ]
          .filter(Boolean)
          .join("\n"),
      );
    });
    if (hiddenTests > 0)
      sections.push(
        theme.fg("muted", `${hiddenTests} callers in tests hidden — includeTests: true shows them`),
      );
    return sections;
  }

  const hops = asRecords(record.hops);
  const sections = [
    joinNonEmpty([
      theme.fg("success", `${hops.length} hop${hops.length === 1 ? "" : "s"}`),
      asBoolean(record.depth_limited) ? theme.fg("warning", "(depth limited)") : undefined,
    ]),
  ];
  if (hops.length === 0) sections.push(theme.fg("muted", "No data-flow hops found."));
  hops.forEach((hop, index) => {
    const file = shortenPath(asString(hop.file) ?? "(unknown file)");
    const symbol = asString(hop.symbol) ?? "(unknown)";
    const variable = asString(hop.variable) ?? "(unknown)";
    const line = asNumber(hop.line) ?? 0;
    const approximate = hop.approximate === true ? ` ${theme.fg("warning", "[approx]")}` : "";
    const nameMatch = nameMatchEdgeMarker(hop, theme);
    sections.push(
      treeLine(
        index,
        `${variable} ${theme.fg("muted", `${asString(hop.flow_type) ?? "flow"}`)} ${symbol} [${file}:${line}]${approximate}${nameMatch}`,
      ),
    );
  });
  return sections;
}
