/**
 * Renderer coverage for AST search/replace Pi tool views.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderAstCall, renderAstResult } from "../tools/ast.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("AST renderers", () => {
  test("renderAstCall shows search and replace summaries", () => {
    const search = renderToString(
      renderAstCall(
        "ast_grep_search",
        { pattern: "console.log($MSG)", lang: "typescript" },
        mockTheme,
        makeContext({ pattern: "console.log($MSG)", lang: "typescript" }),
      ),
    );
    const replace = renderToString(
      renderAstCall(
        "ast_grep_replace",
        { pattern: "console.log($MSG)", rewrite: "logger.info($MSG)", lang: "typescript" },
        mockTheme,
        makeContext({
          pattern: "console.log($MSG)",
          rewrite: "logger.info($MSG)",
          lang: "typescript",
        }),
      ),
    );

    expect(search).toContain("ast search");
    expect(search).toContain("console.log($MSG)");
    expect(replace).toContain("ast replace");
    expect(replace).toContain("logger.info($MSG)");
  });

  test("renderAstResult shows structured matches and diffs", () => {
    const search = renderToString(
      renderAstResult(
        "ast_grep_search",
        makeResult("", {
          matches: [
            {
              file: "/repo/src/a.ts",
              line: 4,
              column: 2,
              text: "console.log(alpha)",
              meta_variables: { $MSG: "alpha" },
            },
          ],
          total_matches: 1,
          files_with_matches: 1,
          files_searched: 2,
        }),
        mockTheme,
        makeContext({ pattern: "console.log($MSG)", lang: "typescript" }),
      ),
    );
    const replace = renderToString(
      renderAstResult(
        "ast_grep_replace",
        makeResult("", {
          dry_run: true,
          total_replacements: 1,
          total_files: 1,
          files: [
            {
              file: "/repo/src/a.ts",
              replacements: 1,
              diff: [
                "--- a/src/a.ts",
                "+++ b/src/a.ts",
                "@@ -1 +1 @@",
                "-console.log(alpha)",
                "+logger.info(alpha)",
              ].join("\n"),
            },
          ],
        }),
        mockTheme,
        makeContext({
          pattern: "console.log($MSG)",
          rewrite: "logger.info($MSG)",
          lang: "typescript",
        }),
      ),
    );

    expect(search).toContain("1 match");
    expect(search).toContain("src/a.ts");
    expect(search).toContain("$MSG = alpha");
    expect(replace).toContain("[dry run]");
    expect(replace).toContain("logger.info(alpha)");
  });

  test("renderAstResult handles error and empty payloads", () => {
    const error = renderToString(
      renderAstResult(
        "ast_grep_search",
        makeResult("bad pattern"),
        mockTheme,
        makeContext({ pattern: "x", lang: "typescript" }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderAstResult(
        "ast_grep_replace",
        makeResult("", { dry_run: true, total_replacements: 0, total_files: 0, files: [] }),
        mockTheme,
        makeContext({ pattern: "x", rewrite: "y", lang: "typescript" }),
      ),
    );

    expect(error).toContain("bad pattern");
    expect(empty).toContain("No files changed");
  });
});
