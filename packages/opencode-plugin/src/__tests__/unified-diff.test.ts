/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";

// Re-import the diff helper through an internal entry. The function is not
// exported from hoisted.ts (it's an implementation detail), so we test it
// via its observable output through the apply_patch metadata flow in
// e2e tests. For unit-level coverage we mirror its behaviour by importing
// the test-only re-export added in hoisted-internals.ts.
//
// Why a dedicated unit suite: the previous implementation was a naive
// line-by-line index comparison that emitted the entire rest of the file
// as "changed" any time a single line was inserted or deleted. That bug
// (issue #22) shipped in v0.15.3 and is exactly the kind of regression a
// unit test catches immediately.
import { _buildUnifiedDiffForTest as buildUnifiedDiff } from "../tools/hoisted-internals.js";

function diffBody(diff: string): string {
  // Strip the constant header so we can assert on the meaningful part.
  const lines = diff.split("\n");
  const headerEnd = lines.findIndex((l) => l.startsWith("@@"));
  if (headerEnd === -1) return "";
  return lines.slice(headerEnd).join("\n");
}

describe("buildUnifiedDiff", () => {
  test("single-line insertion produces a localized hunk, not whole-file diff", () => {
    // Build a 2000-line file. Insert one line near the top.
    const before = Array.from({ length: 2000 }, (_, i) => `line ${i + 1}`).join("\n");
    const inserted = Array.from({ length: 2000 }, (_, i) => {
      if (i === 4) return `line 5\nINSERTED`;
      return `line ${i + 1}`;
    }).join("\n");

    const diff = buildUnifiedDiff("file.ts", before, inserted);
    const body = diffBody(diff);

    // The hunk should contain ONE insertion, surrounded by ~3 lines of
    // context on each side. Total body should be < 20 lines, NOT ~2000.
    expect(body.split("\n").length).toBeLessThan(20);
    expect(body).toContain("+INSERTED");

    // Count actual change markers — there should be exactly 1 + and 0 -.
    const additions = body.split("\n").filter((l) => l.startsWith("+")).length;
    const deletions = body.split("\n").filter((l) => l.startsWith("-")).length;
    expect(additions).toBe(1);
    expect(deletions).toBe(0);
  });

  test("single-line deletion produces a localized hunk", () => {
    const before = Array.from({ length: 100 }, (_, i) => `line ${i + 1}`).join("\n");
    const after = before
      .split("\n")
      .filter((_, i) => i !== 49)
      .join("\n");

    const diff = buildUnifiedDiff("file.ts", before, after);
    const body = diffBody(diff);

    expect(body.split("\n").length).toBeLessThan(15);
    const additions = body.split("\n").filter((l) => l.startsWith("+")).length;
    const deletions = body.split("\n").filter((l) => l.startsWith("-")).length;
    expect(additions).toBe(0);
    expect(deletions).toBe(1);
    expect(body).toContain("-line 50");
  });

  test("single-line replacement produces a localized hunk", () => {
    const before = Array.from({ length: 100 }, (_, i) => `line ${i + 1}`).join("\n");
    const after = before.replace("line 50", "REPLACED");

    const diff = buildUnifiedDiff("file.ts", before, after);
    const body = diffBody(diff);

    expect(body).toContain("-line 50");
    expect(body).toContain("+REPLACED");
    const additions = body.split("\n").filter((l) => l.startsWith("+")).length;
    const deletions = body.split("\n").filter((l) => l.startsWith("-")).length;
    expect(additions).toBe(1);
    expect(deletions).toBe(1);
  });

  test("two distant changes produce two hunks, not one giant one", () => {
    const before = Array.from({ length: 200 }, (_, i) => `line ${i + 1}`).join("\n");
    const after = before.replace("line 10", "FIRST").replace("line 150", "SECOND");

    const diff = buildUnifiedDiff("file.ts", before, after);
    const hunkCount = diff.split("\n").filter((l) => l.startsWith("@@")).length;
    expect(hunkCount).toBe(2);
  });

  test("two close changes are merged into one hunk", () => {
    const before = Array.from({ length: 100 }, (_, i) => `line ${i + 1}`).join("\n");
    const after = before.replace("line 50", "FIRST").replace("line 51", "SECOND");

    const diff = buildUnifiedDiff("file.ts", before, after);
    const hunkCount = diff.split("\n").filter((l) => l.startsWith("@@")).length;
    expect(hunkCount).toBe(1);
  });

  test("identical content produces no hunks", () => {
    const same = "a\nb\nc\n";
    const diff = buildUnifiedDiff("file.ts", same, same);
    const hunkCount = diff.split("\n").filter((l) => l.startsWith("@@")).length;
    expect(hunkCount).toBe(0);
  });

  test("hunk header line numbers reflect actual position", () => {
    const before = Array.from({ length: 50 }, (_, i) => `line ${i + 1}`).join("\n");
    const after = before.replace("line 25", "CHANGED");

    const diff = buildUnifiedDiff("file.ts", before, after);
    const hunkHeader = diff.split("\n").find((l) => l.startsWith("@@"));
    expect(hunkHeader).toBeDefined();
    // Format: @@ -<beforeStart>,<beforeCount> +<afterStart>,<afterCount> @@
    const match = hunkHeader?.match(/@@ -(\d+),(\d+) \+(\d+),(\d+) @@/);
    expect(match).toBeTruthy();
    if (match) {
      const beforeStart = Number.parseInt(match[1], 10);
      const afterStart = Number.parseInt(match[3], 10);
      // Change is at line 25; with 3 context lines start should be around 22.
      expect(beforeStart).toBeLessThanOrEqual(25);
      expect(beforeStart).toBeGreaterThanOrEqual(20);
      expect(afterStart).toBeLessThanOrEqual(25);
      expect(afterStart).toBeGreaterThanOrEqual(20);
    }
  });

  test("file creation (empty before) shows all lines as additions", () => {
    const diff = buildUnifiedDiff("file.ts", "", "a\nb\nc");
    const body = diffBody(diff);
    const additions = body.split("\n").filter((l) => l.startsWith("+")).length;
    expect(additions).toBe(3);
  });

  test("file deletion (empty after) shows all lines as deletions", () => {
    const diff = buildUnifiedDiff("file.ts", "a\nb\nc", "");
    const body = diffBody(diff);
    const deletions = body.split("\n").filter((l) => l.startsWith("-")).length;
    expect(deletions).toBe(3);
  });

  test("files with too many lines skip diff computation", () => {
    // The cost guard is line count, not bytes — LCS is O(n*m) in lines.
    // A short string with 5001 newlines exceeds the 5000-line cap.
    const tooManyLines = Array.from({ length: 5001 }, () => "x").join("\n");
    const diff = buildUnifiedDiff("file.ts", tooManyLines, "small");
    expect(diff).toContain("(diff skipped");
    expect(diff).toMatch(/file has \d+ lines/);
  });

  test("large-byte minified file with few lines is still diffed", () => {
    // Regression: a 100KB+ but few-line file (e.g., minified bundle, JSON
    // blob, large markdown table) was previously rejected by a byte-size
    // gate even though LCS cost is trivial. Now it's diffed normally.
    const huge_one_line = "x".repeat(150 * 1024);
    const diff = buildUnifiedDiff("bundle.min.js", huge_one_line, `${huge_one_line}\n// added`);
    expect(diff).not.toContain("(diff skipped");
    expect(diff).toContain("@@");
  });

  test("3000-line file under the cap is diffed normally", () => {
    // The user-reported manager.ts case: 3084 lines, ~114 KB. Old byte-size
    // gate skipped it; new line gate diffs it correctly.
    const lines = Array.from({ length: 3000 }, (_, i) => `// line ${i + 1}`);
    const before = lines.join("\n");
    const after = lines.map((l, i) => (i === 1500 ? "// EDITED" : l)).join("\n");
    const diff = buildUnifiedDiff("manager.ts", before, after);
    expect(diff).not.toContain("(diff skipped");
    expect(diff).toContain("@@");
    expect(diff).toContain("// EDITED");
  });

  test("regression: 2000-line file with apply_patch-style 5-line edit", () => {
    // Direct repro of issue #22: a relatively localized patch on a large
    // file previously produced "everything below the change" as the diff.
    const lines = Array.from({ length: 2000 }, (_, i) => `// line ${i + 1}`);
    const before = lines.join("\n");
    const after = lines
      .map((l, i) => {
        if (i >= 99 && i <= 103) return `// EDITED ${i + 1}`;
        return l;
      })
      .join("\n");

    const diff = buildUnifiedDiff("big.ts", before, after);
    // 5 changes + 6 context lines + a couple boundary lines should be
    // under 20 lines of diff body, not 1900+.
    const body = diffBody(diff);
    expect(body.split("\n").length).toBeLessThan(25);
    // Exactly 5 + and 5 - markers.
    const additions = body.split("\n").filter((l) => l.startsWith("+")).length;
    const deletions = body.split("\n").filter((l) => l.startsWith("-")).length;
    expect(additions).toBe(5);
    expect(deletions).toBe(5);
  });
});
