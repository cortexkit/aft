/**
 * Renderer coverage for aft_conflicts.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderConflictCall, renderConflictToolResult } from "../tools/conflicts.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("conflicts renderer", () => {
  test("renderConflictCall shows tool title", () => {
    const output = renderToString(renderConflictCall(mockTheme, makeContext({})));
    expect(output).toContain("conflicts");
  });

  test("renderConflictToolResult formats conflict summary", () => {
    const output = renderToString(
      renderConflictToolResult(
        makeResult(
          [
            "1 file, 2 conflicts",
            "",
            "── src/a.ts [2 conflicts] ──",
            "   1: <<<<<<< HEAD",
            "   2: ours",
            "   3: =======",
            "   4: theirs",
            "   5: >>>>>>> main",
          ].join("\n"),
        ),
        mockTheme,
        makeContext({}),
      ),
    );

    expect(output).toContain("1 conflicted file");
    expect(output).toContain("src/a.ts");
    expect(output).toContain("<<<<<<< HEAD");
  });

  test("renderConflictToolResult handles error and empty text", () => {
    const error = renderToString(
      renderConflictToolResult(
        makeResult("git error"),
        mockTheme,
        makeContext({}, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderConflictToolResult(makeResult(""), mockTheme, makeContext({})),
    );

    expect(error).toContain("git error");
    expect(empty).toContain("No merge conflicts found");
  });
});
