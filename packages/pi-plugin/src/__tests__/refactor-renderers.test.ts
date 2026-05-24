/**
 * Renderer coverage for aft_refactor.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderRefactorCall, renderRefactorResult } from "../tools/refactor.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("refactor renderer", () => {
  test("renderRefactorCall shows op and file", () => {
    const output = renderToString(
      renderRefactorCall(
        { op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" },
        mockTheme,
        makeContext({ op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" }),
      ),
    );
    expect(output).toContain("refactor");
    expect(output).toContain("move");
    expect(output).toContain("run");
  });

  test("renderRefactorResult formats move/extract/inline summaries", () => {
    const move = renderToString(
      renderRefactorResult(
        makeResult("", {
          files_modified: 2,
          consumers_updated: 1,
          results: [{ file: "src/a.ts" }, { file: "src/b.ts" }],
        }),
        { op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" },
        mockTheme,
        makeContext({ op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" }),
      ),
    );
    const extract = renderToString(
      renderRefactorResult(
        makeResult("", {
          file: "src/a.ts",
          name: "compute",
          parameters: ["x", "y"],
          return_type: "expression",
        }),
        { op: "extract", filePath: "src/a.ts", name: "compute", startLine: 2, endLine: 4 },
        mockTheme,
        makeContext({
          op: "extract",
          filePath: "src/a.ts",
          name: "compute",
          startLine: 2,
          endLine: 4,
        }),
      ),
    );
    const inline = renderToString(
      renderRefactorResult(
        makeResult("", {
          file: "src/a.ts",
          symbol: "helper",
          call_context: "expression",
          substitutions: 2,
        }),
        { op: "inline", filePath: "src/a.ts", symbol: "helper", callSiteLine: 8 },
        mockTheme,
        makeContext({ op: "inline", filePath: "src/a.ts", symbol: "helper", callSiteLine: 8 }),
      ),
    );

    expect(move).toContain("moved symbol run");
    expect(move).toContain("files modified 2");
    expect(extract).toContain("extracted compute");
    expect(inline).toContain("inlined helper");
  });

  test("renderRefactorResult handles error and empty payloads", () => {
    const empty = renderToString(
      renderRefactorResult(
        makeResult("", {}),
        { op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" },
        mockTheme,
        makeContext({ op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" }),
      ),
    );
    const error = renderToString(
      renderRefactorResult(
        makeResult("ambiguous symbol"),
        { op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" },
        mockTheme,
        makeContext(
          { op: "move", filePath: "src/a.ts", symbol: "run", destination: "src/b.ts" },
          { isError: true },
        ),
      ),
    );

    expect(empty).toContain("moved symbol run");
    expect(error).toContain("ambiguous symbol");
  });
});
