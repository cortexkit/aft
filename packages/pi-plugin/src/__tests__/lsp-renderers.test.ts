/**
 * Renderer coverage for lsp_diagnostics.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderDiagnosticsCall, renderDiagnosticsResult } from "../tools/lsp.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("lsp renderer", () => {
  test("renderDiagnosticsCall shows target path", () => {
    const output = renderToString(
      renderDiagnosticsCall(
        { filePath: "src/a.ts", severity: "error" },
        mockTheme,
        makeContext({ filePath: "src/a.ts", severity: "error" }),
      ),
    );
    expect(output).toContain("lsp diagnostics");
    expect(output).toContain("src/a.ts");
  });

  test("renderDiagnosticsResult groups diagnostics by file", () => {
    const output = renderToString(
      renderDiagnosticsResult(
        makeResult("", {
          diagnostics: [
            {
              file: "/repo/src/a.ts",
              line: 4,
              column: 2,
              severity: "error",
              code: "TS2322",
              message: "bad type",
            },
            { file: "/repo/src/a.ts", line: 8, column: 1, severity: "warning", message: "unused" },
          ],
          total: 2,
          files_with_errors: 1,
        }),
        mockTheme,
        makeContext({ filePath: "src/a.ts" }),
      ),
    );

    expect(output).toContain("2 diagnostics");
    expect(output).toContain("src/a.ts");
    expect(output).toContain("4:2 TS2322 bad type");
  });

  test("renderDiagnosticsResult handles error and empty payloads", () => {
    const error = renderToString(
      renderDiagnosticsResult(
        makeResult("tsserver unavailable"),
        mockTheme,
        makeContext({ directory: "." }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderDiagnosticsResult(
        makeResult("", { diagnostics: [], total: 0, files_with_errors: 0 }),
        mockTheme,
        makeContext({ directory: "." }),
      ),
    );

    expect(error).toContain("tsserver unavailable");
    expect(empty).toContain("No diagnostics found");
  });
});
