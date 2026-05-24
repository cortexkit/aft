/**
 * Renderer coverage for aft_delete + aft_move.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderFsCall, renderFsResult } from "../tools/fs.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("fs renderers", () => {
  test("renderFsCall shows delete and move paths", () => {
    const del = renderToString(
      renderFsCall(
        "aft_delete",
        { files: ["src/a.ts"] },
        mockTheme,
        makeContext({ files: ["src/a.ts"] }),
      ),
    );
    const move = renderToString(
      renderFsCall(
        "aft_move",
        { filePath: "src/a.ts", destination: "src/b.ts" },
        mockTheme,
        makeContext({ filePath: "src/a.ts", destination: "src/b.ts" }),
      ),
    );

    expect(del).toContain("delete");
    expect(del).toContain("src/a.ts");
    expect(move).toContain("move");
    expect(move).toContain("src/b.ts");
  });

  test("renderFsCall summarizes multi-file deletes by count", () => {
    const out = renderToString(
      renderFsCall(
        "aft_delete",
        { files: ["a.ts", "b.ts", "c.ts"] },
        mockTheme,
        makeContext({ files: ["a.ts", "b.ts", "c.ts"] }),
      ),
    );
    expect(out).toContain("delete");
    expect(out).toContain("3");
  });

  test("renderFsResult shows delete and move success summaries", () => {
    const del = renderToString(
      renderFsResult(
        "aft_delete",
        { files: ["src/a.ts"] },
        makeResult("Deleted src/a.ts", {
          success: true,
          complete: true,
          deleted: ["src/a.ts"],
          skipped_files: [],
        }),
        mockTheme,
        makeContext({ files: ["src/a.ts"] }),
      ),
    );
    const move = renderToString(
      renderFsResult(
        "aft_move",
        { filePath: "src/a.ts", destination: "src/b.ts" },
        makeResult("Moved src/a.ts → src/b.ts"),
        mockTheme,
        makeContext({ filePath: "src/a.ts", destination: "src/b.ts" }),
      ),
    );

    expect(del).toContain("deleted");
    expect(move).toContain("moved");
    expect(move).toContain("src/b.ts");
  });

  test("renderFsResult shows partial-delete with skipped reasons", () => {
    const out = renderToString(
      renderFsResult(
        "aft_delete",
        { files: ["a.ts", "missing.ts"] },
        makeResult("Deleted 1/2 file(s)", {
          success: true,
          complete: false,
          deleted: ["a.ts"],
          skipped_files: [{ file: "missing.ts", reason: "path_not_found" }],
        }),
        mockTheme,
        makeContext({ files: ["a.ts", "missing.ts"] }),
      ),
    );
    expect(out).toContain("✓ deleted");
    expect(out).toContain("a.ts");
    expect(out).toContain("✗ skipped");
    expect(out).toContain("missing.ts");
    expect(out).toContain("path_not_found");
  });

  test("renderFsResult handles error and missing payloads", () => {
    const error = renderToString(
      renderFsResult(
        "aft_delete",
        { files: ["src/a.ts"] },
        makeResult("permission denied"),
        mockTheme,
        makeContext({ files: ["src/a.ts"] }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderFsResult(
        "aft_move",
        { filePath: "src/a.ts", destination: "src/b.ts" },
        makeResult(""),
        mockTheme,
        makeContext({ filePath: "src/a.ts", destination: "src/b.ts" }),
      ),
    );

    expect(error).toContain("permission denied");
    expect(empty).toContain("src/b.ts");
  });
});
