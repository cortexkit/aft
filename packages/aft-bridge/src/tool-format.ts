/** Shared pure formatting helpers for AFT host plugins. */

function asPlainObject(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

function candidateLocation(candidate: Record<string, unknown>): string | undefined {
  const file =
    typeof candidate.file === "string" && candidate.file.length > 0 ? candidate.file : undefined;
  if (!file) return undefined;
  const line =
    typeof candidate.line === "number" && Number.isFinite(candidate.line)
      ? candidate.line
      : undefined;
  return line === undefined ? file : `${file}:${line}`;
}

function stringifyData(data: unknown): string | undefined {
  if (data === undefined) return undefined;
  try {
    return JSON.stringify(data, null, 2);
  } catch {
    return String(data);
  }
}

/** Format bridge failure envelopes without dropping structured error data. */
export function formatBridgeErrorMessage(
  command: string,
  response: Record<string, unknown>,
  params: Record<string, unknown> = {},
): string {
  const code =
    typeof response.code === "string" && response.code.length > 0 ? response.code : undefined;
  const message =
    typeof response.message === "string" && response.message.length > 0
      ? response.message
      : `${command} failed`;
  // Rust merges error_with_data() extras into the top-level response, NOT under
  // a nested `data` field. Read structured fields at top-level first; fall back
  // to `response.data` for forward-compat with any handler that uses nesting.
  const data = asPlainObject(response.data);
  const rawCandidates = Array.isArray(response.candidates)
    ? response.candidates
    : Array.isArray(data?.candidates)
      ? data.candidates
      : undefined;
  const rawSymbol =
    typeof response.symbol === "string" && response.symbol.length > 0
      ? response.symbol
      : typeof data?.symbol === "string" && data.symbol.length > 0
        ? data.symbol
        : undefined;

  if (code === "ambiguous_target" || code === "target_symbol_not_in_file") {
    const candidates = (rawCandidates ?? [])
      .map(asPlainObject)
      .filter((candidate): candidate is Record<string, unknown> => candidate !== undefined)
      .map(candidateLocation)
      .filter((candidate): candidate is string => candidate !== undefined);

    if (candidates.length > 0) {
      const symbol =
        typeof params.toSymbol === "string" && params.toSymbol.length > 0
          ? params.toSymbol
          : rawSymbol;
      const target = symbol ? `multiple symbols named "${symbol}"` : message.replace(/[.!?]+$/, "");
      const action =
        code === "ambiguous_target"
          ? "Pass toPath to disambiguate"
          : "Try one of these files for toPath";
      return `${command}: ${code} — ${target}. ${action}:\n${candidates
        .map((candidate) => `  - ${candidate}`)
        .join("\n")}`;
    }
  }

  if (!code) return message;

  const lines = [`${command}: ${code} — ${message}`];
  // For unhandled structured error codes, surface any extra fields beyond
  // code/message/success/id so agents see the full context (not just data.*).
  const extras = collectStructuredExtras(response);
  if (extras) lines.push(`data: ${extras}`);
  return lines.join("\n");
}

/**
 * Capture any structured fields a Rust error_with_data() merged into the top-level
 * response, excluding the well-known envelope keys (id/success/code/message) and
 * already-shown nested `data` (handled separately when present).
 */
function collectStructuredExtras(response: Record<string, unknown>): string | undefined {
  // status_bar is transport metadata attached to EVERY bridge response (the
  // [AFT ...] health bar) — never error context. Without this exclusion every
  // structured error dumped the raw status-bar JSON as `data:`.
  const reserved = new Set([
    "id",
    "success",
    "code",
    "message",
    "data",
    "status_bar",
    "bg_completions",
  ]);
  const extras: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(response)) {
    if (reserved.has(key)) continue;
    extras[key] = value;
  }
  if (Object.keys(extras).length === 0) {
    return stringifyData(response.data);
  }
  // Prefer top-level extras; fold any nested data fields beneath.
  if (response.data !== undefined) extras.data = response.data;
  return stringifyData(extras);
}

export interface ReadFooterOptions {
  /** Host-specific parameter hint, for example `startLine/endLine` or `offset/limit`. */
  rangeHint: string;
}

/** Build the navigation footer for a clamped `read` response. */
export function formatReadFooter(
  agentSpecifiedRange: boolean,
  data: Record<string, unknown>,
  options: ReadFooterOptions,
): string {
  // CASE B: agent picked the range. No footer at all. They have the math.
  if (agentSpecifiedRange) return "";

  if (!data.truncated) return "";

  const startLine = data.start_line as number | undefined;
  const endLine = data.end_line as number | undefined;
  const totalLines = data.total_lines as number | undefined;
  if (startLine === undefined || endLine === undefined || totalLines === undefined) {
    return "";
  }

  // CASE A: agent did not pick a range, response was clamped — hint is
  // useful, tell them how to read more.
  return `\n(Showing lines ${startLine}-${endLine} of ${totalLines}. Use ${options.rangeHint} to read other sections.)`;
}
