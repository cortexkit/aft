import { describe, expect, test } from "bun:test";
import { formatEditSummary } from "../edit-summary.js";

describe("formatEditSummary", () => {
  test("single find/replace reports counts only, no path/JSON", () => {
    expect(formatEditSummary({ replacements: 1, diff: { additions: 3, deletions: 2 } })).toBe(
      "Edited (+3/-2).",
    );
  });

  test("omits replacement count when <= 1", () => {
    expect(formatEditSummary({ replacements: 1, diff: { additions: 1, deletions: 1 } })).toBe(
      "Edited (+1/-1).",
    );
  });

  test("surfaces replacement count when > 1 (replaceAll)", () => {
    expect(formatEditSummary({ replacements: 3, diff: { additions: 3, deletions: 0 } })).toBe(
      "Edited (+3/-0, 3 replacements).",
    );
  });

  test("names fuzzy tier and actual non-whitespace replacement text", () => {
    expect(
      formatEditSummary({
        replacements: 2,
        fuzzy_match: { pass: 3, replaced_text: ["| old | b |", "| old | b |"] },
      }),
    ).toContain(
      "Fuzzy match (pass 3).\nReplaced text 1: | old | b |\nReplaced text 2: | old | b |",
    );
  });

  test("reports fuzzy batch occurrences without repeating unchanged whitespace", () => {
    const summary = formatEditSummary({
      fuzzy_matches: [
        { pass: 5, replaced_text: [] },
        { pass: 4, replaced_text: ["old punctuation"] },
      ],
    });
    expect(summary).toContain("Fuzzy match (pass 5).");
    expect(summary).toContain("Replaced text 1: old punctuation");
  });

  test("renders the producer's loose-match notice once per distinct notice", () => {
    const notice =
      "oldString matched only after ignoring indentation and trailing whitespace; newString was re-indented to match the file.";
    expect(
      formatEditSummary({
        edits_applied: 1,
        diff: { additions: 2, deletions: 2 },
        fuzzy_matches: [
          { pass: 3, replaced_text: [], notice },
          { pass: 3, replaced_text: [], notice },
        ],
      }),
    ).toBe(`Edited (+2/-2).\n${notice}`);
  });

  test("surfaces edits_applied for batch mode when > 1", () => {
    expect(formatEditSummary({ edits_applied: 2, diff: { additions: 4, deletions: 1 } })).toBe(
      "Edited (+4/-1, 2 edits).",
    );
  });

  test("missing diff defaults counts to zero", () => {
    expect(formatEditSummary({ replacements: 1 })).toBe("Edited (+0/-0).");
  });

  test("created file uses the Created headline", () => {
    expect(formatEditSummary({ created: true, diff: { additions: 10, deletions: 0 } })).toBe(
      "Created file (+10/-0).",
    );
  });

  test("appends Auto-formatted when formatted is true", () => {
    expect(
      formatEditSummary({ replacements: 1, formatted: true, diff: { additions: 1, deletions: 1 } }),
    ).toBe("Edited (+1/-1). Auto-formatted.");
    expect(
      formatEditSummary({ created: true, formatted: true, diff: { additions: 2, deletions: 0 } }),
    ).toBe("Created file (+2/-0). Auto-formatted.");
  });

  test("surfaces reformatted.text with reflow message", () => {
    const s = formatEditSummary({
      replacements: 1,
      formatted: true,
      diff: { additions: 2, deletions: 1 },
      reformatted: { text: "fn main() {\n    let x = 1;\n}" },
    });
    expect(s).toContain("reflowed your edit");
    expect(s).toContain("fn main()");
    expect(s).not.toMatch(/\. Auto-formatted\.$/);
  });

  test("surfaces reformatted.extensive with re-read hint", () => {
    expect(
      formatEditSummary({
        replacements: 1,
        formatted: true,
        diff: { additions: 1, deletions: 1 },
        reformatted: { extensive: true },
      }),
    ).toBe(
      "Edited (+1/-1). Auto-formatted — extensive reflow; re-read the file before your next anchored edit.",
    );
  });

  test("transaction reports file count, singular/plural", () => {
    expect(formatEditSummary({ files_modified: 1 })).toBe("Applied edits to 1 file.");
    expect(formatEditSummary({ files_modified: 2 })).toBe("Applied edits to 2 files.");
  });

  test("rollback is honest: never claims the edit applied", () => {
    const s = formatEditSummary({
      rolled_back: true,
      replacements: 1,
      diff: { additions: 1, deletions: 1 },
    });
    expect(s).toContain("rolled back");
    expect(s).toContain("left unchanged");
    expect(s).not.toContain("Edited (");
  });

  test("rollback takes precedence over files_modified", () => {
    expect(formatEditSummary({ rolled_back: true, files_modified: 2 })).toContain("rolled back");
  });

  test("glob edit reports file + replacement counts (not a misleading +0/-0)", () => {
    expect(formatEditSummary({ total_files: 3, total_replacements: 7 })).toBe(
      "Edited 3 files (7 replacements).",
    );
    expect(formatEditSummary({ total_files: 1, total_replacements: 1 })).toBe(
      "Edited 1 file (1 replacement).",
    );
    // missing total_replacements defaults to 0 without falling through to diff.
    expect(formatEditSummary({ total_files: 2 })).toBe("Edited 2 files (0 replacements).");
  });
});
