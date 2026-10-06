/**
 * Plain-text formatter for `aft_zoom` responses.
 *
 * Both @cortexkit/aft-opencode and @cortexkit/aft-pi consume the same Rust
 * zoom command response shape — keep one formatter so both hosts produce
 * byte-identical output.
 *
 * The previous output was JSON.stringify of the raw response, which left
 * the agent decoding `\n` and `\"` escapes to read the source. The new
 * format inlines content with line numbers and only renders annotation
 * sections when non-empty.
 *
 * Output shape (single symbol):
 *
 *   src/foo.ts:8-43 [function resolveParentContext]
 *
 *      5: import { resolveMessageContext } from "...";
 *      6: import { getSessionAgent } from "...";
 *      7:
 *      8: export async function resolveParentContext(
 *      9:   ctx: ToolContextWithMetadata,
 *     ...
 *     43: }
 *
 *   ──── calls_out
 *     resolveMessageContext (line 13)
 *     getSessionAgent (line 16)
 *
 *   ──── called_by
 *     handleTaskRequest (line 87)
 *
 * Annotation sections are omitted when empty. Context-before/after lines are
 * included when present (their line numbers continue the gutter).
 */

interface RangeShape {
  start_line: number;
  end_line: number;
  start_col?: number;
  end_col?: number;
}

interface CallRefShape {
  name: string;
  line: number;
  extra_count?: number;
}

interface AnnotationsShape {
  calls_out?: CallRefShape[];
  called_by?: CallRefShape[];
  other_calls?: number;
}

/**
 * Subset of the Rust ZoomResponse shape this formatter cares about. Extra
 * fields (id, command, success, ...) are ignored.
 */
export interface ZoomResponseLike {
  name?: string;
  kind?: string;
  range?: RangeShape;
  content?: string;
  context_before?: string[];
  context_after?: string[];
  annotations?: AnnotationsShape;
}

function formatExtraCallSites(ref: CallRefShape): string {
  return Number.isInteger(ref.extra_count) && (ref.extra_count ?? 0) > 0
    ? ` +${ref.extra_count}`
    : "";
}

/**
 * Format a single Rust zoom response as plain text.
 *
 * `targetLabel` is what the agent passed in (filePath or url) — used for the
 * header. Avoids leaking internal cache paths when the agent zoomed into a URL.
 */
export function formatZoomText(targetLabel: string, response: ZoomResponseLike): string {
  const range = response.range;
  const startLine = range?.start_line ?? 1;
  const endLine = range?.end_line ?? startLine;
  const kind = response.kind ?? "symbol";
  const name = response.name ?? "";
  const contentText = typeof response.content === "string" ? response.content : "";
  const ctxBefore = Array.isArray(response.context_before) ? response.context_before : [];
  const ctxAfter = Array.isArray(response.context_after) ? response.context_after : [];

  // Header. For "lines" kind (range fallback when no symbol matches) drop the
  // redundant "[lines lines X-Y]" decoration and just show path:start-end.
  const header =
    kind === "lines"
      ? `${targetLabel}:${startLine}-${endLine}`
      : `${targetLabel}:${startLine}-${endLine} [${kind} ${name}]`.trimEnd();

  // Strip a trailing empty line introduced by content.split("\n") on a body
  // that ends with "\n". Real zoom content always ends with the symbol's
  // closing brace + a newline.
  const contentLines = contentText.split("\n");
  if (contentLines.length > 0 && contentLines[contentLines.length - 1] === "") {
    contentLines.pop();
  }

  const lastDisplayedLine = endLine + ctxAfter.length;
  const gutterWidth = String(Math.max(lastDisplayedLine, 1)).length;
  const fmt = (lineNo: number, text: string) => `${String(lineNo).padStart(gutterWidth)}: ${text}`;

  const out: string[] = [header, ""];

  // context_before sits BEFORE startLine. ctxBefore.length entries → numbers
  // start at startLine - ctxBefore.length and run up to startLine - 1.
  let lineNo = startLine - ctxBefore.length;
  for (const text of ctxBefore) {
    out.push(fmt(lineNo, text));
    lineNo += 1;
  }
  for (const text of contentLines) {
    out.push(fmt(lineNo, text));
    lineNo += 1;
  }
  for (const text of ctxAfter) {
    out.push(fmt(lineNo, text));
    lineNo += 1;
  }

  // Annotations (only when non-empty)
  const callsOut = response.annotations?.calls_out ?? [];
  const calledBy = response.annotations?.called_by ?? [];
  const otherCalls = response.annotations?.other_calls ?? 0;
  if (callsOut.length > 0 || otherCalls > 0) {
    out.push("", "──── calls_out");
    for (const ref of callsOut) {
      out.push(`  ${ref.name} (line ${ref.line})${formatExtraCallSites(ref)}`);
    }
    if (otherCalls > 0) {
      out.push(`  +${otherCalls} other calls`);
    }
  }
  if (calledBy.length > 0) {
    out.push("", "──── called_by");
    for (const ref of calledBy) {
      out.push(`  ${ref.name} (line ${ref.line})${formatExtraCallSites(ref)}`);
    }
  }

  return out.join("\n");
}

/**
 * Per-target entry for {@link formatZoomMultiTargetResult}.
 *
 * `targetLabel` is what the agent passed in (filePath or url) and is used for
 * the per-section header.
 * `name` is the symbol the agent asked for (used in the failure-line wording).
 * `response` is the Rust zoom response (may be `success: false` on failure).
 */
export interface ZoomMultiTargetEntry {
  targetLabel: string;
  name: string;
  response: { success?: boolean; message?: unknown } & Record<string, unknown>;
}

/** Single rendered entry from a multi-target zoom call. */
export interface ZoomMultiTargetSymbolResult {
  targetLabel: string;
  name: string;
  success: boolean;
  content?: string;
  error?: string;
}

/** Aggregate result of a multi-target zoom call. */
export interface ZoomMultiTargetResult {
  complete: boolean;
  entries: ZoomMultiTargetSymbolResult[];
  text: string;
}

/**
 * Format multi-target zoom results as plain text. Each successful entry uses
 * {@link formatZoomText} with its OWN `targetLabel` (so cross-file batches
 * still show the right file path in each section). Failures render as
 * `Symbol "name" not found in <file>: <reason>`.
 *
 * Sections are blank-line separated and output is byte-identical across hosts.
 */
export function formatZoomMultiTargetResult(
  entries: ZoomMultiTargetEntry[],
): ZoomMultiTargetResult {
  const rendered = entries.map((entry): ZoomMultiTargetSymbolResult => {
    const { targetLabel, name, response } = entry;
    if (response.success === false) {
      const message =
        typeof response.message === "string" && response.message.length > 0
          ? response.message
          : "zoom failed";
      return { targetLabel, name, success: false, error: message };
    }
    return {
      targetLabel,
      name,
      success: true,
      content: formatZoomText(targetLabel, response as ZoomResponseLike),
    };
  });
  const complete = rendered.every((entry) => entry.success);
  const sections: string[] = [];
  if (!complete) {
    sections.push("Incomplete zoom results: one or more symbols failed.");
  }
  for (const entry of rendered) {
    if (entry.success) {
      sections.push(entry.content ?? "");
    } else {
      sections.push(
        `Symbol "${entry.name}" not found in ${entry.targetLabel}: ${entry.error ?? "zoom failed"}`,
      );
    }
  }
  return { complete, entries: rendered, text: sections.join("\n\n") };
}

/** One entry in a Rust-side multi-symbol zoom batch (`zoom_batch_symbols`). */
export interface RustZoomBatchEntry {
  name: string;
  response: { success?: boolean; message?: unknown } & Record<string, unknown>;
}

/**
 * True when the bridge returned the Rust batch envelope from a single zoom request
 * (e.g. whitespace-split `symbol` on a code file).
 */
export function isRustZoomBatchEnvelope(
  response: Record<string, unknown>,
): response is Record<string, unknown> & { symbols: RustZoomBatchEntry[] } {
  if (!Array.isArray(response.symbols) || response.symbols.length === 0) {
    return false;
  }
  return response.symbols.every((entry) => {
    if (!entry || typeof entry !== "object") return false;
    const row = entry as Record<string, unknown>;
    return typeof row.name === "string" && row.response !== undefined && row.response !== null;
  });
}

/**
 * Unwrap a Rust batch envelope into parallel name/response arrays for
 * {@link formatZoomBatchResult} (plugin-local) or the same shaping in hosts.
 */
export function unwrapRustZoomBatchEnvelope(response: Record<string, unknown>): {
  names: string[];
  responses: Record<string, unknown>[];
} | null {
  if (!isRustZoomBatchEnvelope(response)) {
    return null;
  }
  const names: string[] = [];
  const responses: Record<string, unknown>[] = [];
  for (const entry of response.symbols) {
    names.push(entry.name);
    responses.push(entry.response as Record<string, unknown>);
  }
  return { names, responses };
}
