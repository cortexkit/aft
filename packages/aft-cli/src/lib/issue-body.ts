/**
 * Helpers for shaping `aft doctor --issue` GitHub issue bodies within
 * GitHub's hard ~64KB issue-body limit.
 *
 * Two responsibilities live here:
 *
 *   1. Error-line extraction — pull the most-recent ERROR-shaped lines from
 *      a sanitized log so the issue body has a dedicated `## Recent errors`
 *      section that survives even when the main log tail needs aggressive
 *      truncation.
 *
 *   2. GitHub byte-budget capping — when a rendered report exceeds the
 *      budget, shrink the main log block (the noise-heavy section) from
 *      the top, preserving the diagnostics / configuration / error sections
 *      that matter most.
 *
 * Both helpers operate on already-sanitized markdown so they're harness-
 * agnostic — OpenCode and Pi share the same byte budget, the same
 * truncation marker text, and the same precision/false-positive tradeoff
 * on what counts as an "error" line.
 */

/**
 * GitHub issue body byte budget. GitHub enforces ~64KB (65536 bytes); we
 * leave 4KB of headroom for: GH's own URL encoding when opening the
 * "Submit new issue" tab via `gh issue create --web`, future minor
 * markdown growth from new sections, and a safety margin against any
 * single-line entry crossing the cap.
 */
export const MAX_GITHUB_BODY_BYTES = 60_000;

/**
 * Pattern tokens that mark a log line as ERROR-shaped. AFT's runtime uses
 * a small, predictable vocabulary in `sessionLog(...)` calls and Rust
 * `slog_*` macros across both OpenCode and Pi plugins: `failed:`, typed
 * `Error:` shapes, `EMERGENCY`, `panicked at`, `exception`. We also pick
 * up stack-frame lines (`    at SomeFn (file:line:col)` and Rust panic
 * frames) so the agent reading the issue sees enough context to identify
 * the call site.
 *
 * The match requires colon-suffixed keywords (`failed:` not `failed`) OR
 * the explicit `Error`/`EMERGENCY`/`panicked` words to qualify. That
 * precision avoids the common false positive where a log message includes
 * "failed" as past-tense status (e.g. `compiled 12 files; 0 failed` is
 * telemetry, not an error).
 */
const ERROR_LOG_PATTERNS = [
  // Common AFT sessionLog / slog_warn shapes:
  //   "bridge crashed: ..." / "X crashed:"
  /\bcrashed:/i,
  // Common AFT sessionLog / slog_warn shapes:
  //   "configure failed: ..." / "X failed:"
  /\bfailed:/i,
  // Standard JS/TS Error / typed-error rendering: "Error: msg", "TypeError: ...":
  /\b(?:[A-Z][a-zA-Z]*)?Error:\s/,
  // Rust panic header from RUST_BACKTRACE=1 / regular panic dumps:
  /\bpanicked at\b/,
  // Emergency abort path emits ALL CAPS marker:
  /\bEMERGENCY\b/,
  // Generic exception/throw text:
  /\bexception\b/i,
  // V8/JSC stack-trace frames (kept so the agent gets call-site context
  // next to the failure line itself):
  /^\s+at\s+[\w.<>$]+\s+\(/,
  // Bare "    at file:line" frames (no function name):
  /^\s+at\s+(?:file:|node_modules\/|[^/\s]+:\d+)/,
];

function isErrorLogLine(line: string): boolean {
  return ERROR_LOG_PATTERNS.some((rx) => rx.test(line));
}

/**
 * Extract the most-recent error-shaped log lines from a sanitized log.
 * Returns them in chronological order (oldest match first → newest match
 * last) so the issue body reads naturally top-to-bottom.
 *
 * **Why this exists**: GitHub issue bodies have a hard ~64KB limit and a
 * busy session's tail can easily blow past that. If the body needs
 * truncation, the error section MUST survive because the whole point of
 * the issue is the error. This extractor pulls them into a separate
 * section that the body-cap is careful not to drop.
 */
export function extractRecentErrors(sanitized: string, limit = 20): string[] {
  const matches: string[] = [];
  const lines = sanitized.split(/\r?\n/);
  // Walk newest-first, stop once we hit `limit`.
  for (let i = lines.length - 1; i >= 0 && matches.length < limit; i -= 1) {
    if (isErrorLogLine(lines[i])) {
      matches.push(lines[i]);
    }
  }
  return matches.reverse();
}

/**
 * Apply a byte budget to a rendered issue body. If the body is already
 * within budget, returns it unchanged. Otherwise rewrites the main
 * fenced log block (whose heading must start with `## Logs (last`) to
 * drop oldest log lines until the body fits, leaving a clear
 * `[truncated for GitHub 64KB limit — older log lines dropped]` marker
 * at the top of the kept slice.
 *
 * This deliberately ONLY touches the main-log section — the description,
 * environment, diagnostics, and recent-errors sections are preserved
 * intact because they're the most useful parts of the report. The main
 * log is the noise-heavy one and the right thing to shrink first.
 *
 * AFT's main log section contains per-harness sub-sections (`#### opencode
 * log (path)\n```...```\n#### pi log (path)\n```...```\n`). The cap walks
 * the entire `## Logs (last ...` block as a single shrinkable region
 * because precise per-harness allocation is fiddly and the alternative —
 * dropping the whole log block on overflow — is strictly worse for
 * debugging.
 *
 * Returns the (possibly-shrunk) body. UTF-8 byte length is the budget,
 * matching how GitHub measures issue bodies (the issue API rejects
 * `body` payloads above the limit).
 */
export function capBodyToGithubLimit(
  body: string,
  maxBytes: number = MAX_GITHUB_BODY_BYTES,
): string {
  if (Buffer.byteLength(body, "utf8") <= maxBytes) return body;

  // Anchor on the AFT main-log heading. We don't search for closing
  // markers directly because there are multiple fenced blocks inside the
  // logs section (one per harness); instead we treat the whole section
  // from this heading to the next `## ` (top-level) heading as the
  // shrinkable region. If there's no following top-level heading, we
  // shrink to end-of-body.
  const heading = "## Logs (last";
  const headingIdx = body.indexOf(heading);
  if (headingIdx === -1) {
    // No main log section to shrink — fall back to a raw byte truncation
    // with a marker. This shouldn't happen for issues generated by the
    // doctor flow, but keeps the function defensive for callers passing
    // arbitrary markdown.
    const marker = "\n\n[truncated for GitHub 64KB limit]\n";
    const markerBytes = Buffer.byteLength(marker, "utf8");
    // Slice the body to a code-point boundary so we never split a multi-
    // byte UTF-8 character. Naive `Buffer.subarray(...).toString("utf8")`
    // would replace any half-codepoint at the cut with U+FFFD (3 bytes),
    // pushing the output OVER the requested budget.
    return truncateToByteBudget(body, maxBytes - markerBytes) + marker;
  }
  // Find the end of the heading line (the newline immediately after it).
  const headingEol = body.indexOf("\n", headingIdx);
  if (headingEol === -1) return body; // malformed; pass through unchanged
  // Find the next top-level heading after the logs section. If none, the
  // logs section runs to end-of-body.
  const nextSectionIdx = findNextTopLevelHeading(body, headingEol + 1);
  const logBlockStart = headingEol + 1;
  const logBlockEnd = nextSectionIdx === -1 ? body.length : nextSectionIdx;

  const head = body.slice(0, logBlockStart);
  const log = body.slice(logBlockStart, logBlockEnd);
  const tail = body.slice(logBlockEnd);

  const overheadBytes = Buffer.byteLength(head, "utf8") + Buffer.byteLength(tail, "utf8");
  // Reserve room for the truncation marker that we'll prepend to the log
  // body so the agent / human reading the issue knows lines were dropped.
  const truncationMarker = "[truncated for GitHub 64KB limit — older log lines dropped]\n";
  const markerBytes = Buffer.byteLength(truncationMarker, "utf8");
  const logBudget = maxBytes - overheadBytes - markerBytes;
  if (logBudget <= 0) {
    // Even with no log content we'd be over budget. Drop the log block
    // entirely (keep the heading + a stub marker) so the rest survives.
    return `${head}${truncationMarker}${tail}`;
  }

  // Drop oldest lines (from the top) until what's left fits the budget.
  // We split on newlines to preserve line boundaries; binary truncation
  // would corrupt the final line.
  const lines = log.split("\n");
  let keepLines = lines;
  let kept = keepLines.join("\n");
  while (Buffer.byteLength(kept, "utf8") > logBudget && keepLines.length > 1) {
    // Drop ~5% from the top each iteration for fast convergence on
    // very-oversized inputs. Caps at "drop at least one line".
    const dropCount = Math.max(1, Math.floor(keepLines.length * 0.05));
    keepLines = keepLines.slice(dropCount);
    kept = keepLines.join("\n");
  }
  // Final defensive byte-truncation in case a single huge line still
  // overshoots (e.g. one log line that itself exceeds the budget).
  if (Buffer.byteLength(kept, "utf8") > logBudget) {
    kept = truncateToByteBudget(kept, logBudget);
  }

  return `${head}${truncationMarker}${kept}${tail}`;
}

/**
 * Find the next top-level `## ` heading after `startIdx`. Returns the
 * byte offset of the `#` character, or -1 if none found.
 *
 * Used by `capBodyToGithubLimit` to bound the logs section. We require
 * the heading to be at the start of a line (preceded by `\n` or at the
 * very start of the body) and start with EXACTLY `## ` (two hashes + space)
 * so we don't accidentally match `### ` subheadings, code-fence content
 * containing `#`, or `## Logs (last...` itself.
 */
function findNextTopLevelHeading(body: string, startIdx: number): number {
  // `\n## ` (newline + two hashes + space) anchors a top-level heading
  // at the start of a line and excludes `### ` / `#### ` subheadings
  // because the next char after the second `#` must be a space.
  const idx = body.indexOf("\n## ", startIdx);
  return idx === -1 ? -1 : idx + 1; // skip the leading \n
}

/**
 * Truncate a string to at most `maxBytes` UTF-8 bytes WITHOUT splitting a
 * multi-byte code point. Naive `Buffer.subarray(...).toString("utf8")`
 * replaces any partial codepoint at the cut with U+FFFD (3 bytes), which
 * paradoxically pushes the output OVER the requested budget when the cut
 * happens mid-character.
 *
 * Approach: take the raw byte slice, then walk back from the end until the
 * trailing byte is a UTF-8 start byte (`0xxxxxxx` or `11xxxxxx`). Drop any
 * leading-byte position whose codepoint can't be completed within the
 * budget. This always lands on a clean codepoint boundary and never grows
 * the output past `maxBytes`.
 */
function truncateToByteBudget(input: string, maxBytes: number): string {
  if (maxBytes <= 0) return "";
  const buf = Buffer.from(input, "utf8");
  if (buf.length <= maxBytes) return input;
  let end = maxBytes;
  // Walk back to the nearest UTF-8 codepoint boundary. UTF-8 continuation
  // bytes are 10xxxxxx; start bytes are 0xxxxxxx or 11xxxxxx.
  while (end > 0 && (buf[end] & 0b1100_0000) === 0b1000_0000) {
    end -= 1;
  }
  return buf.subarray(0, end).toString("utf8");
}
