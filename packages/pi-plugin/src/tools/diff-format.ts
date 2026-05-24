/**
 * Diff formatting helpers for Pi hoisted tool renderers.
 *
 * Mirrors the output shape Pi's built-in `renderDiff` expects:
 *   "+NN content"   for added lines
 *   "-NN content"   for removed lines
 *   " NN content"   for context lines
 *   " NN ..."       for context truncation markers
 *
 * We don't re-export Pi's internal `generateDiffString` because it isn't
 * public. Reimplementing it as a thin wrapper around the `diff` npm package
 * (also used by Pi itself) is ~40 lines and keeps us decoupled from Pi's
 * private internals.
 */

import { diffLines } from "diff";

export interface FormattedDiff {
  diff: string;
  firstChangedLine: number | undefined;
}

const DEFAULT_CONTEXT_LINES = 4;

/**
 * Generate a line-numbered diff string suitable for Pi's `renderDiff`.
 * Matches the format of Pi's built-in edit tool result.
 */
export function formatDiffForPi(
  oldContent: string,
  newContent: string,
  contextLines = DEFAULT_CONTEXT_LINES,
): FormattedDiff {
  const parts = diffLines(oldContent, newContent);
  const output: string[] = [];

  const oldLines = oldContent.split("\n");
  const newLines = newContent.split("\n");
  const maxLineNum = Math.max(oldLines.length, newLines.length);
  const lineNumWidth = String(maxLineNum).length;
  const pad = (n: number): string => String(n).padStart(lineNumWidth, " ");
  const blank = " ".repeat(lineNumWidth);

  let oldLineNum = 1;
  let newLineNum = 1;
  let lastWasChange = false;
  let firstChangedLine: number | undefined;

  for (let i = 0; i < parts.length; i++) {
    const part = parts[i];
    const raw = part.value.split("\n");
    if (raw[raw.length - 1] === "") raw.pop();

    if (part.added || part.removed) {
      if (firstChangedLine === undefined) firstChangedLine = newLineNum;
      for (const line of raw) {
        if (part.added) {
          output.push(`+${pad(newLineNum)} ${line}`);
          newLineNum++;
        } else {
          output.push(`-${pad(oldLineNum)} ${line}`);
          oldLineNum++;
        }
      }
      lastWasChange = true;
      continue;
    }

    // Context.
    const nextIsChange = i < parts.length - 1 && (parts[i + 1].added || parts[i + 1].removed);
    const hasLeading = lastWasChange;
    const hasTrailing = nextIsChange;

    if (hasLeading && hasTrailing) {
      if (raw.length <= contextLines * 2) {
        for (const line of raw) {
          output.push(` ${pad(oldLineNum)} ${line}`);
          oldLineNum++;
          newLineNum++;
        }
      } else {
        for (const line of raw.slice(0, contextLines)) {
          output.push(` ${pad(oldLineNum)} ${line}`);
          oldLineNum++;
          newLineNum++;
        }
        const skipped = raw.length - contextLines * 2;
        output.push(` ${blank} ...`);
        oldLineNum += skipped;
        newLineNum += skipped;
        for (const line of raw.slice(raw.length - contextLines)) {
          output.push(` ${pad(oldLineNum)} ${line}`);
          oldLineNum++;
          newLineNum++;
        }
      }
    } else if (hasLeading) {
      const shown = raw.slice(0, contextLines);
      for (const line of shown) {
        output.push(` ${pad(oldLineNum)} ${line}`);
        oldLineNum++;
        newLineNum++;
      }
      const skipped = raw.length - shown.length;
      if (skipped > 0) {
        output.push(` ${blank} ...`);
        oldLineNum += skipped;
        newLineNum += skipped;
      }
    } else if (hasTrailing) {
      // Using slice(-contextLines) is unsafe: slice(-0) === slice(0) returns
      // the full array. Compute the positive start offset explicitly so that
      // contextLines === 0 collapses to an empty shown list (matching Pi).
      const shownCount = Math.min(contextLines, raw.length);
      const shown = raw.slice(raw.length - shownCount);
      const skipped = raw.length - shown.length;
      if (skipped > 0) {
        output.push(` ${blank} ...`);
        oldLineNum += skipped;
        newLineNum += skipped;
      }
      for (const line of shown) {
        output.push(` ${pad(oldLineNum)} ${line}`);
        oldLineNum++;
        newLineNum++;
      }
    } else {
      oldLineNum += raw.length;
      newLineNum += raw.length;
    }
    lastWasChange = false;
  }

  return { diff: output.join("\n"), firstChangedLine };
}
