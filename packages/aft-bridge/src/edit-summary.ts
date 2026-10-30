// Shared, compact agent-facing summary for file-mutation tool results
// (edit / write / apply_patch). Both the OpenCode and Pi plugins
// render the SAME agent-facing text from this helper so the two harnesses stay
// in parity.
//
// Design (see session decision): the agent already supplied the path and the
// content it wants, so we do NOT echo the file path, the before/after content,
// or the raw Rust JSON envelope back to the model. Doing so makes the payload
// scale with file size, not edit size. The rich data (diff body, backup id,
// status-bar counts, etc.) stays in the plugin's UI `metadata`/`details`; the
// status-bar line is injected separately by the bridge layer.
//
// The model only needs: did it apply, how much changed (cheap confirmation),
// and any signal it must act on (rollback, no-op, format-skip, LSP errors).

/** Shape of the Rust mutation response fields this helper reads. */
export interface EditSummaryInput {
  /** Files modified in a multi-file write (Rust `files_modified`). */
  files_modified?: number;
  /** Total replacements across a glob edit (Rust `total_replacements`). */
  total_replacements?: number;
  /** Total files touched by a glob edit (Rust `total_files`). */
  total_files?: number;
  /** Number of match replacements (find/replace, replaceAll). */
  replacements?: number;
  /** Number of edits applied in batch mode (Rust `edits_applied`). */
  edits_applied?: number;
  fuzzy_match?: { pass: number; replaced_text: string[]; notice?: string };
  fuzzy_matches?: Array<{ pass: number; replaced_text: string[]; notice?: string }>;
  /** True when a new file was created (append/write create path). */
  created?: boolean;
  /** True when the post-write content is byte-identical to before. */
  no_op?: boolean;
  /** True when the write was reverted (e.g. generated invalid syntax). */
  rolled_back?: boolean;
  /** Whether the file was auto-formatted after the write. */
  formatted?: boolean;
  /** Post-format reflow excerpt from Rust when the formatter changed the applied edit. */
  reformatted?: { text?: string; extensive?: boolean };
  /** Diff counts. before/after content is intentionally ignored here. */
  diff?: { additions?: number; deletions?: number };
}

/**
 * Build the compact agent-facing summary line for a successful mutation.
 *
 * Returns just the headline sentence; callers append their own conditional
 * notes (no-op, format-skip, LSP diagnostics, pending/exited servers) which
 * already exist per-plugin and carry real signal.
 *
 * Honesty: when `rolled_back` is true the change did NOT land, so we never say
 * "applied" — we say the file was left unchanged. This was previously buried
 * inside a raw `"rolled_back":false` JSON field the agent had to parse.
 */
export function formatEditSummary(data: EditSummaryInput): string {
  // Rollback is the one case where "applied" would be a lie. The write was
  // reverted (typically because the result failed syntax validation), so the
  // file is unchanged and the agent must retry differently.
  if (data.rolled_back === true) {
    return "Edit rolled back: the change produced invalid syntax, so the file was left unchanged.";
  }

  // Multi-file write: report file count, not per-file diffs (those are in the
  // UI metadata).
  if (typeof data.files_modified === "number") {
    const n = data.files_modified;
    return `Applied edits to ${n} file${n === 1 ? "" : "s"}.`;
  }

  // Glob edit (filePath is a glob pattern): Rust reports per-file results plus
  // `total_replacements`/`total_files` rather than a top-level diff. Without
  // this the headline would fall through to a misleading `Edited (+0/-0).`.
  if (typeof data.total_files === "number") {
    const files = data.total_files;
    const reps = data.total_replacements ?? 0;
    return `Edited ${files} file${files === 1 ? "" : "s"} (${reps} replacement${reps === 1 ? "" : "s"}).`;
  }

  const additions = data.diff?.additions ?? 0;
  const deletions = data.diff?.deletions ?? 0;
  const counts = `+${additions}/-${deletions}`;

  // Append/write create path.
  if (data.created === true) {
    let s = `Created file (${counts}).`;
    if (data.formatted) s += formatAutoFormattedSuffix(data);
    return s;
  }

  // Batch mode reports edits_applied; replaceAll reports replacements > 1.
  let detail = counts;
  if (typeof data.edits_applied === "number" && data.edits_applied > 1) {
    detail = `${counts}, ${data.edits_applied} edits`;
  } else if (typeof data.replacements === "number" && data.replacements > 1) {
    detail = `${counts}, ${data.replacements} replacements`;
  }

  let s = `Edited (${detail}).`;
  if (data.formatted) s += formatAutoFormattedSuffix(data);
  const fuzzy = data.fuzzy_matches ?? (data.fuzzy_match ? [data.fuzzy_match] : []);
  // The notice wording comes from the Rust producer (fuzzy_match.rs
  // loose_match_notice); older producers without it get the bare pass number.
  // A notice identical to the previous one is not repeated, matching the Rust
  // reply formatter.
  let previousNotice: string | undefined;
  for (const match of fuzzy) {
    const notice = match.notice ?? `Fuzzy match (pass ${match.pass}).`;
    if (notice !== previousNotice) s += `\n${notice}`;
    previousNotice = notice;
    for (const [index, text] of match.replaced_text.entries()) {
      s += `\nReplaced text ${index + 1}: ${text}`;
    }
  }
  return s;
}

function formatAutoFormattedSuffix(data: EditSummaryInput): string {
  const reflowText = data.reformatted?.text;
  if (typeof reflowText === "string" && reflowText.length > 0) {
    return `\nAuto-formatted — the formatter reflowed your edit. On disk now:\n${reflowText}`;
  }
  if (data.reformatted?.extensive === true) {
    return " Auto-formatted — extensive reflow; re-read the file before your next anchored edit.";
  }
  return " Auto-formatted.";
}
