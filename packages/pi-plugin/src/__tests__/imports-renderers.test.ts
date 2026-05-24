/**
 * Renderer coverage for aft_import.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderImportCall, renderImportResult } from "../tools/imports.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("import renderer", () => {
  test("renderImportCall shows op and module", () => {
    const output = renderToString(
      renderImportCall(
        { op: "add", filePath: "src/a.ts", module: "react", names: ["useState"] },
        mockTheme,
        makeContext({ op: "add", filePath: "src/a.ts", module: "react", names: ["useState"] }),
      ),
    );
    expect(output).toContain("import");
    expect(output).toContain("add");
    expect(output).toContain("react");
  });

  test("renderImportResult shows add and organize summaries", () => {
    const add = renderToString(
      renderImportResult(
        makeResult("", { file: "src/a.ts", module: "react", group: "external", added: true }),
        { op: "add", filePath: "src/a.ts", module: "react" },
        mockTheme,
        makeContext({ op: "add", filePath: "src/a.ts", module: "react" }),
      ),
    );
    const organize = renderToString(
      renderImportResult(
        makeResult("", {
          file: "src/a.ts",
          groups: [
            { name: "external", count: 1 },
            { name: "internal", count: 2 },
          ],
          removed_duplicates: 3,
        }),
        { op: "organize", filePath: "src/a.ts" },
        mockTheme,
        makeContext({ op: "organize", filePath: "src/a.ts" }),
      ),
    );

    expect(add).toContain("added react");
    expect(organize).toContain("organized src/a.ts");
    expect(organize).toContain("duplicates removed 3");
  });

  test("renderImportResult handles error and empty payloads", () => {
    const error = renderToString(
      renderImportResult(
        makeResult("bad module"),
        { op: "remove", filePath: "src/a.ts", module: "react" },
        mockTheme,
        makeContext({ op: "remove", filePath: "src/a.ts", module: "react" }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderImportResult(
        makeResult("", {}),
        { op: "organize", filePath: "src/a.ts" },
        mockTheme,
        makeContext({ op: "organize", filePath: "src/a.ts" }),
      ),
    );

    expect(error).toContain("bad module");
    expect(empty).toContain("No imports found");
  });
});
