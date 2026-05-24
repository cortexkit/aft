/**
 * Minimal patch parser for the opencode `*** Begin Patch` format.
 * Ported from opencode's internal Patch module.
 */

export interface AddHunk {
  type: "add";
  path: string;
  contents: string;
}

export interface DeleteHunk {
  type: "delete";
  path: string;
}

export interface UpdateFileChunk {
  old_lines: string[];
  new_lines: string[];
  change_context?: string;
  is_end_of_file?: boolean;
}

export interface UpdateHunk {
  type: "update";
  path: string;
  move_path?: string;
  chunks: UpdateFileChunk[];
}

export type Hunk = AddHunk | DeleteHunk | UpdateHunk;

function stripHeredoc(input: string): string {
  const heredocMatch = input.match(/^(?:cat\s+)?<<['"]?(\w+)['"]?\s*\n([\s\S]*?)\n\1\s*$/);
  return heredocMatch ? heredocMatch[2] : input;
}

function parsePatchHeader(
  lines: string[],
  startIdx: number,
): { filePath: string; movePath?: string; nextIdx: number } | null {
  const line = lines[startIdx];

  if (line.startsWith("*** Add File:")) {
    const filePath = line.slice("*** Add File:".length).trim();
    return filePath ? { filePath, nextIdx: startIdx + 1 } : null;
  }

  if (line.startsWith("*** Delete File:")) {
    const filePath = line.slice("*** Delete File:".length).trim();
    return filePath ? { filePath, nextIdx: startIdx + 1 } : null;
  }

  if (line.startsWith("*** Update File:")) {
    const filePath = line.slice("*** Update File:".length).trim();
    let movePath: string | undefined;
    let nextIdx = startIdx + 1;

    if (nextIdx < lines.length && lines[nextIdx].startsWith("*** Move to:")) {
      movePath = lines[nextIdx].slice("*** Move to:".length).trim();
      nextIdx++;
    }

    return filePath ? { filePath, movePath, nextIdx } : null;
  }

  return null;
}

function parseAddFileContent(
  lines: string[],
  startIdx: number,
): { content: string; nextIdx: number } {
  let content = "";
  let i = startIdx;

  while (i < lines.length && !lines[i].startsWith("***")) {
    if (lines[i].startsWith("+")) {
      content += `${lines[i].substring(1)}\n`;
    }
    i++;
  }

  if (content.endsWith("\n")) {
    content = content.slice(0, -1);
  }

  return { content, nextIdx: i };
}

function parseUpdateFileChunks(
  lines: string[],
  startIdx: number,
): { chunks: UpdateFileChunk[]; nextIdx: number } {
  const chunks: UpdateFileChunk[] = [];
  let i = startIdx;

  while (i < lines.length && !lines[i].startsWith("***")) {
    if (lines[i].startsWith("@@")) {
      const contextLine = lines[i].substring(2).trim();
      i++;

      const oldLines: string[] = [];
      const newLines: string[] = [];
      let isEndOfFile = false;

      while (i < lines.length && !lines[i].startsWith("@@") && !lines[i].startsWith("***")) {
        const changeLine = lines[i];

        if (changeLine === "*** End of File") {
          isEndOfFile = true;
          i++;
          break;
        }

        if (changeLine.startsWith(" ")) {
          const content = changeLine.substring(1);
          oldLines.push(content);
          newLines.push(content);
        } else if (changeLine.startsWith("-")) {
          oldLines.push(changeLine.substring(1));
        } else if (changeLine.startsWith("+")) {
          newLines.push(changeLine.substring(1));
        }

        i++;
      }

      chunks.push({
        old_lines: oldLines,
        new_lines: newLines,
        change_context: contextLine || undefined,
        is_end_of_file: isEndOfFile || undefined,
      });
    } else {
      i++;
    }
  }

  return { chunks, nextIdx: i };
}

/** Maximum patch text size in bytes to prevent memory exhaustion. */
const MAX_PATCH_SIZE = 1024 * 1024; // 1 MB
/** Maximum number of hunks (file operations) per patch. */
const MAX_HUNKS = 500;

export function parsePatch(patchText: string): Hunk[] {
  if (patchText.length > MAX_PATCH_SIZE) {
    throw new Error(
      `Patch too large: ${patchText.length} bytes exceeds limit of ${MAX_PATCH_SIZE} bytes`,
    );
  }

  const cleaned = stripHeredoc(patchText.trim());
  const lines = cleaned.split("\n");
  const hunks: Hunk[] = [];

  const beginIdx = lines.findIndex((line) => line.trim() === "*** Begin Patch");
  const endIdx = lines.findIndex((line) => line.trim() === "*** End Patch");

  if (beginIdx === -1 || endIdx === -1 || beginIdx >= endIdx) {
    throw new Error("Invalid patch format: missing *** Begin Patch / *** End Patch markers");
  }

  let i = beginIdx + 1;

  while (i < endIdx) {
    const header = parsePatchHeader(lines, i);
    if (!header) {
      i++;
      continue;
    }

    if (hunks.length >= MAX_HUNKS) {
      throw new Error(`Patch exceeds maximum of ${MAX_HUNKS} file operations`);
    }

    if (lines[i].startsWith("*** Add File:")) {
      const { content, nextIdx } = parseAddFileContent(lines, header.nextIdx);
      hunks.push({ type: "add", path: header.filePath, contents: content });
      i = nextIdx;
    } else if (lines[i].startsWith("*** Delete File:")) {
      hunks.push({ type: "delete", path: header.filePath });
      i = header.nextIdx;
    } else if (lines[i].startsWith("*** Update File:")) {
      const { chunks, nextIdx } = parseUpdateFileChunks(lines, header.nextIdx);
      hunks.push({
        type: "update",
        path: header.filePath,
        move_path: header.movePath,
        chunks,
      });
      i = nextIdx;
    } else {
      i++;
    }
  }

  return hunks;
}

// ---------------------------------------------------------------------------
// Apply update chunks to file content
// ---------------------------------------------------------------------------

function normalizeUnicode(str: string): string {
  return str
    .replace(/[\u2018\u2019\u201A\u201B]/g, "'")
    .replace(/[\u201C\u201D\u201E\u201F]/g, '"')
    .replace(/[\u2010\u2011\u2012\u2013\u2014\u2015]/g, "-")
    .replace(/\u2026/g, "...")
    .replace(/\u00A0/g, " ");
}

/**
 * Normalize leading-whitespace indentation: collapse any mix of tabs and
 * spaces at the start of a line to a single canonical space sequence,
 * with each tab counting as one space (the size doesn't matter — both
 * sides are normalized identically). Catches the common drift where the
 * model emits 2-space indents for a file that uses tabs (or 4-space).
 *
 * Trailing/middle whitespace is left alone — only leading runs are
 * touched, and only on the lines that have leading whitespace (so empty
 * lines compare unchanged).
 */
function normalizeIndent(str: string): string {
  return str.replace(/^[\t ]+/, (m) => " ".repeat(m.length));
}

type Comparator = (a: string, b: string) => boolean;

function tryMatch(
  lines: string[],
  pattern: string[],
  startIndex: number,
  compare: Comparator,
  eof: boolean,
): number {
  if (eof) {
    const fromEnd = lines.length - pattern.length;
    if (fromEnd >= startIndex) {
      let matches = true;
      for (let j = 0; j < pattern.length; j++) {
        if (!compare(lines[fromEnd + j], pattern[j])) {
          matches = false;
          break;
        }
      }
      if (matches) return fromEnd;
    }
  }

  for (let i = startIndex; i <= lines.length - pattern.length; i++) {
    let matches = true;
    for (let j = 0; j < pattern.length; j++) {
      if (!compare(lines[i + j], pattern[j])) {
        matches = false;
        break;
      }
    }
    if (matches) return i;
  }

  return -1;
}

/**
 * The fuzzy-match ladder for `apply_patch` chunk lines.
 *
 * Each step is more permissive than the last. We return at the first
 * successful step so the most-specific match wins. The order matters:
 * `exact` first means a clean patch never gets stretched to match
 * something that just looks similar.
 *
 * Returned `tier` is exposed for diagnostics (BUG-6b): when none of these
 * match, we tell the agent which strictness levels we tried so they
 * understand the actual drift class.
 *
 * Tiers (least → most permissive):
 *   "exact"   — byte-for-byte
 *   "rstrip"  — trailing whitespace drift (CRLF, trailing tabs/spaces)
 *   "trim"    — leading + trailing whitespace drift
 *   "indent"  — tab vs space leading whitespace, sizes ignored
 *               (BUG-6c: catches "model emitted 2-space indents for a
 *                tab-indented file" or vice versa)
 *   "unicode" — smart quotes, em dashes, ellipsis, NBSP normalization
 */
type MatchTier = "exact" | "rstrip" | "trim" | "indent" | "unicode";

function seekSequenceTiered(
  lines: string[],
  pattern: string[],
  startIndex: number,
  eof = false,
): { found: number; tier: MatchTier } | null {
  if (pattern.length === 0) return null;

  const exact = tryMatch(lines, pattern, startIndex, (a, b) => a === b, eof);
  if (exact !== -1) return { found: exact, tier: "exact" };

  const rstrip = tryMatch(lines, pattern, startIndex, (a, b) => a.trimEnd() === b.trimEnd(), eof);
  if (rstrip !== -1) return { found: rstrip, tier: "rstrip" };

  const trim = tryMatch(lines, pattern, startIndex, (a, b) => a.trim() === b.trim(), eof);
  if (trim !== -1) return { found: trim, tier: "trim" };

  // Indent-normalized: collapse leading [\t ]+ to a uniform space run on
  // BOTH sides, then re-trim trailing space (caught by rstrip already, but
  // belt-and-suspenders for mixed tab+trail-space cases). Trailing
  // whitespace and middle whitespace are otherwise preserved so that
  // intra-line drift (e.g. extra spaces around an operator) is NOT
  // silently merged — that's a real semantic difference.
  const indent = tryMatch(
    lines,
    pattern,
    startIndex,
    (a, b) => normalizeIndent(a).trimEnd() === normalizeIndent(b).trimEnd(),
    eof,
  );
  if (indent !== -1) return { found: indent, tier: "indent" };

  const unicode = tryMatch(
    lines,
    pattern,
    startIndex,
    (a, b) => normalizeUnicode(a.trim()) === normalizeUnicode(b.trim()),
    eof,
  );
  if (unicode !== -1) return { found: unicode, tier: "unicode" };

  return null;
}

/**
 * Backwards-compatible single-int return preserving the original API.
 * New callers prefer seekSequenceTiered to get the matched tier.
 */
function seekSequence(lines: string[], pattern: string[], startIndex: number, eof = false): number {
  const r = seekSequenceTiered(lines, pattern, startIndex, eof);
  return r ? r.found : -1;
}

/**
 * Find the file location whose first line is the closest match to the
 * pattern's first line, scoring by how many CONSECUTIVE leading lines
 * also match (under any tier in the ladder). Used purely for diagnostics
 * (BUG-6b) — we never accept a partial match for the actual edit, only
 * surface "looks like the closest candidate is here, where it diverges
 * on line N". Returns null if pattern[0] doesn't appear anywhere under
 * any tier.
 *
 * Scope is bounded — we scan the whole file but only score the top N
 * candidates to avoid quadratic cost on big files with very common
 * lines (e.g. blank `}` lines).
 */
function findClosestPartialMatch(
  lines: string[],
  pattern: string[],
): { lineNumber: number; matchedLines: number; firstDivergence: number } | null {
  if (pattern.length === 0 || lines.length === 0) return null;

  // Find candidate starting lines whose first line matches pattern[0]
  // under any tier. Cap to 16 candidates so we don't burn time on
  // pathological cases (file full of `}`).
  const compareAny = (a: string, b: string) =>
    a === b ||
    a.trimEnd() === b.trimEnd() ||
    a.trim() === b.trim() ||
    normalizeIndent(a).trimEnd() === normalizeIndent(b).trimEnd() ||
    normalizeUnicode(a.trim()) === normalizeUnicode(b.trim());

  const candidates: number[] = [];
  for (let i = 0; i < lines.length && candidates.length < 16; i++) {
    if (compareAny(lines[i], pattern[0])) candidates.push(i);
  }
  if (candidates.length === 0) return null;

  // Score each candidate by how many consecutive leading lines match.
  let best = { lineNumber: -1, matchedLines: 0, firstDivergence: -1 };
  for (const start of candidates) {
    let matched = 0;
    for (let j = 0; j < pattern.length && start + j < lines.length; j++) {
      if (!compareAny(lines[start + j], pattern[j])) break;
      matched++;
    }
    if (matched > best.matchedLines) {
      best = {
        lineNumber: start + 1, // 1-based for the agent
        matchedLines: matched,
        firstDivergence: matched, // 0-based offset into pattern
      };
    }
  }
  return best.lineNumber === -1 ? null : best;
}

/**
 * Apply update chunks to the original file content, producing new content.
 * Ported from opencode's deriveNewContentsFromChunks.
 */
export function applyUpdateChunks(
  originalContent: string,
  filePath: string,
  chunks: UpdateFileChunk[],
): string {
  const originalLines = originalContent.split("\n");

  if (originalLines.length > 0 && originalLines[originalLines.length - 1] === "") {
    originalLines.pop();
  }

  const replacements: Array<[number, number, string[]]> = [];
  let lineIndex = 0;

  for (const chunk of chunks) {
    if (chunk.change_context) {
      const contextIdx = seekSequence(originalLines, [chunk.change_context], lineIndex);
      if (contextIdx === -1) {
        throw new Error(`Failed to find context '${chunk.change_context}' in ${filePath}`);
      }
      lineIndex = contextIdx + 1;
    }

    if (chunk.old_lines.length === 0) {
      const insertionIdx =
        originalLines.length > 0 && originalLines[originalLines.length - 1] === ""
          ? originalLines.length - 1
          : originalLines.length;
      replacements.push([insertionIdx, 0, chunk.new_lines]);
      continue;
    }

    let pattern = chunk.old_lines;
    let newSlice = chunk.new_lines;
    let found = seekSequence(originalLines, pattern, lineIndex, chunk.is_end_of_file);

    if (found === -1 && pattern.length > 0 && pattern[pattern.length - 1] === "") {
      pattern = pattern.slice(0, -1);
      if (newSlice.length > 0 && newSlice[newSlice.length - 1] === "") {
        newSlice = newSlice.slice(0, -1);
      }
      found = seekSequence(originalLines, pattern, lineIndex, chunk.is_end_of_file);
    }

    if (found !== -1) {
      replacements.push([found, pattern.length, newSlice]);
      lineIndex = found + pattern.length;
    } else {
      // Diagnose: did the agent send a hunk whose REWRITE is already in the
      // file? That happens when an agent partially applied the patch in a
      // prior turn (e.g. via `edit`) and then re-runs `apply_patch` against
      // an already-mutated file. Surface that explicitly so the agent
      // doesn't waste a turn re-reading the file to figure it out.
      const newSliceTrimmed = newSlice.filter((line) => line.trim().length > 0);
      const alreadyApplied =
        newSliceTrimmed.length > 0 &&
        seekSequence(originalLines, newSliceTrimmed, 0, chunk.is_end_of_file) !== -1;

      // BUG-6b diagnostic: tell the agent WHERE in the file we got
      // closest, and which line first diverged. Without this they have
      // to bash `grep -n` to figure out where their hunk thought it was
      // pointing.
      const closest = findClosestPartialMatch(originalLines, pattern);
      let closestHint = "";
      if (closest && closest.matchedLines > 0) {
        const fileLineNo = closest.lineNumber + closest.firstDivergence;
        const expectedLine = pattern[closest.firstDivergence];
        const actualLine =
          fileLineNo - 1 < originalLines.length ? originalLines[fileLineNo - 1] : "<EOF>";
        closestHint =
          `\n\nClosest match starts at line ${closest.lineNumber} ` +
          `(${closest.matchedLines} of ${pattern.length} lines matched).\n` +
          `First divergence at line ${fileLineNo}:\n` +
          `  expected: ${JSON.stringify(expectedLine)}\n` +
          `  actual:   ${JSON.stringify(actualLine)}`;
      }

      // Tell the agent WHICH normalizations we already tried so they
      // know what kinds of drift the matcher already tolerates and
      // don't waste a turn re-emitting the patch with whitespace
      // tweaks that wouldn't help.
      const triedTiers = "exact, trimEnd, trim, indent (tab/space), unicode";

      const alreadyAppliedHint = alreadyApplied
        ? "\n\nHint: the replacement content for this hunk already appears in the file. " +
          "The patch may have been partially applied in a prior turn — re-read the file " +
          "to confirm which hunks still need to apply."
        : "";

      throw new Error(
        `Failed to find expected lines in ${filePath}:\n${chunk.old_lines.join("\n")}\n\n` +
          `Tried match tiers: ${triedTiers}.${closestHint}${alreadyAppliedHint}`,
      );
    }
  }

  replacements.sort((a, b) => a[0] - b[0]);

  const result = [...originalLines];
  for (let i = replacements.length - 1; i >= 0; i--) {
    const [startIdx, oldLen, newSegment] = replacements[i];
    result.splice(startIdx, oldLen, ...newSegment);
  }

  if (result.length === 0 || result[result.length - 1] !== "") {
    result.push("");
  }

  return result.join("\n");
}
