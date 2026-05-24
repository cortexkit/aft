import { describe, expect, test } from "bun:test";
import { applyUpdateChunks, parsePatch, type UpdateFileChunk } from "../patch-parser.js";

describe("parsePatch error paths", () => {
  test("throws when begin/end markers are missing", () => {
    expect(() => parsePatch("*** Add File: hello.txt\n+hello")).toThrow(
      "Invalid patch format: missing *** Begin Patch / *** End Patch markers",
    );
  });

  test("returns an empty list for an empty patch body", () => {
    expect(parsePatch("*** Begin Patch\n*** End Patch")).toEqual([]);
  });

  test("ignores malformed add-file headers with no filename", () => {
    expect(parsePatch("*** Begin Patch\n*** Add File:\n+hello\n*** End Patch")).toEqual([]);
  });

  test("throws when patch text exceeds the size limit", () => {
    const oversizedPatch = "x".repeat(1024 * 1024 + 1);

    expect(() => parsePatch(oversizedPatch)).toThrow(
      `Patch too large: ${oversizedPatch.length} bytes exceeds limit of 1048576 bytes`,
    );
  });

  test("throws when the patch contains more than 500 file operations", () => {
    const patch = ["*** Begin Patch"];
    for (let index = 0; index < 501; index++) {
      patch.push(`*** Add File: file-${index}.txt`);
      patch.push(`+line ${index}`);
    }
    patch.push("*** End Patch");

    expect(() => parsePatch(patch.join("\n"))).toThrow(
      "Patch exceeds maximum of 500 file operations",
    );
  });

  test("treats invalid heredoc wrappers as plain text and still parses the enclosed patch", () => {
    const wrappedPatch = [
      "<<EOF",
      "*** Begin Patch",
      "*** Add File: hello.txt",
      "+hello world",
      "*** End Patch",
      "NOT_EOF",
    ].join("\n");

    expect(parsePatch(wrappedPatch)).toEqual([
      { type: "add", path: "hello.txt", contents: "hello world" },
    ]);
    expect(parsePatch(`prefix\n${wrappedPatch}`)).toEqual([
      { type: "add", path: "hello.txt", contents: "hello world" },
    ]);
  });
});

describe("applyUpdateChunks error paths", () => {
  test("throws when change context does not match any line", () => {
    const chunks: UpdateFileChunk[] = [
      {
        change_context: "missing line",
        old_lines: ["beta"],
        new_lines: ["updated beta"],
      },
    ];

    expect(() => applyUpdateChunks("alpha\nbeta\n", "src/example.ts", chunks)).toThrow(
      "Failed to find context 'missing line' in src/example.ts",
    );
  });

  test("throws when old_lines do not exist in the file", () => {
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["missing line"],
        new_lines: ["replacement line"],
      },
    ];

    expect(() => applyUpdateChunks("alpha\nbeta\n", "src/example.ts", chunks)).toThrow(
      "Failed to find expected lines in src/example.ts:\nmissing line",
    );
  });

  test("error includes 'already applied' hint when the new_lines are already in the file", () => {
    // Simulates an agent retrying apply_patch after partially applying earlier:
    // the old line is gone (already replaced), the new line is already present.
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["const mainQuota = await getFreshMainQuota(auth.access, storage)"],
        new_lines: ["const mainQuota = await getMainQuotaForRouting(auth.access, storage)"],
      },
    ];

    const fileWithRewriteAlreadyApplied =
      "alpha\nconst mainQuota = await getMainQuotaForRouting(auth.access, storage)\nbeta\n";

    expect(() =>
      applyUpdateChunks(fileWithRewriteAlreadyApplied, "src/example.ts", chunks),
    ).toThrow(/already appears in the file/);
  });

  test("error has no 'already applied' hint when both old and new lines are absent", () => {
    // Genuinely wrong patch — neither side matches the file.
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["missing old line"],
        new_lines: ["missing new line"],
      },
    ];

    let message = "";
    try {
      applyUpdateChunks("unrelated content\n", "src/example.ts", chunks);
    } catch (e) {
      message = (e as Error).message;
    }
    expect(message).toContain("Failed to find expected lines");
    expect(message).not.toContain("already appears in the file");
  });

  /// BUG-6c (fuzzy match resilience): the model emitted 4-space indents
  /// but the file actually uses a single tab. With the new "indent" tier
  /// in the seekSequence ladder, the patch should still apply.
  test("matches when patch uses spaces but file uses tabs (indent tier)", () => {
    // File has TAB indentation.
    const file = "function foo() {\n\treturn 42;\n}\n";

    // Patch's chunk uses 4-SPACE indentation.
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["    return 42;"],
        new_lines: ["    return 43;"],
      },
    ];

    const result = applyUpdateChunks(file, "src/foo.ts", chunks);

    // Replacement landed: line 2 is now the new content. Note: the
    // replacement uses the patch's whitespace (4 spaces), not the file's
    // tab — this is a known tradeoff of the indent tier. The agent gets
    // a working patch and the file's formatter (biome/prettier) will
    // re-indent on the next save.
    expect(result).toBe("function foo() {\n    return 43;\n}\n");
  });

  /// Inverse: file uses 4-space, patch uses tab. Indent tier handles
  /// drift in either direction.
  test("matches when patch uses tabs but file uses spaces (indent tier, inverse)", () => {
    const file = "function foo() {\n    return 42;\n}\n";
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["\treturn 42;"],
        new_lines: ["\treturn 43;"],
      },
    ];

    const result = applyUpdateChunks(file, "src/foo.ts", chunks);
    expect(result).toBe("function foo() {\n\treturn 43;\n}\n");
  });

  /// BUG-6b (better diagnostics): when the match fails, the error
  /// includes the closest-match line number, how many lines matched,
  /// and the first divergence point. Without this the agent has to
  /// `grep -n` to figure out where their hunk thought it was pointing.
  test("error includes closest-match line and divergence diagnostic (BUG-6b)", () => {
    const file =
      "function foo() {\n  const x = 1;\n  const y = 2;\n  const z = 3;\n  return x + y + z;\n}\n";
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["  const x = 1;", "  const y = 2;", "  const Q = 99;"], // last line drifts
        new_lines: ["  const x = 1;", "  const y = 2;", "  const Q = 100;"],
      },
    ];

    let message = "";
    try {
      applyUpdateChunks(file, "src/foo.ts", chunks);
    } catch (e) {
      message = (e as Error).message;
    }

    // Tells the agent WHICH line the closest match starts at.
    expect(message).toContain("Closest match starts at line 2");
    // Tells them HOW MANY lines matched before divergence.
    expect(message).toContain("2 of 3 lines matched");
    // Tells them WHERE the divergence is.
    expect(message).toContain("First divergence at line 4");
    // Shows expected vs actual at the divergence point so they don't
    // have to reread the file to figure out the drift.
    expect(message).toContain('expected: "  const Q = 99;"');
    expect(message).toContain('actual:   "  const z = 3;"');
  });

  /// BUG-6b: even when there's no plausible closest match (no candidate
  /// line in the file even loosely matches the pattern's first line), the
  /// error still names the tiers we already tried so the agent knows what
  /// kinds of drift they don't need to manually fix.
  test("error lists the match tiers that were tried (BUG-6b)", () => {
    const chunks: UpdateFileChunk[] = [
      {
        old_lines: ["completely unrelated line"],
        new_lines: ["replacement"],
      },
    ];

    let message = "";
    try {
      applyUpdateChunks("alpha\nbeta\ngamma\n", "src/foo.ts", chunks);
    } catch (e) {
      message = (e as Error).message;
    }

    expect(message).toContain("Tried match tiers:");
    expect(message).toContain("exact");
    expect(message).toContain("trim");
    expect(message).toContain("indent");
    expect(message).toContain("unicode");
  });
});
