/**
 * Renderer coverage for aft_outline + aft_zoom.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  formatZoomBatchResult,
  renderOutlineCall,
  renderOutlineResult,
  renderZoomCall,
  renderZoomResult,
} from "../tools/reading.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("reading renderers", () => {
  test("renderOutlineCall and renderZoomCall show targets", () => {
    const outline = renderToString(
      renderOutlineCall({ target: "src/a.ts" }, mockTheme, makeContext({ target: "src/a.ts" })),
    );
    const zoom = renderToString(
      renderZoomCall(
        { filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ filePath: "src/a.ts", symbol: "run" }),
      ),
    );

    expect(outline).toContain("outline");
    expect(outline).toContain("src/a.ts");
    expect(zoom).toContain("zoom");
    expect(zoom).toContain("run");
  });

  test("renderOutlineResult and renderZoomResult show structured output", () => {
    const outline = renderToString(
      renderOutlineResult(
        makeResult("sample.ts\n  E fn run() 1:5\n  - cls Service 7:12"),
        mockTheme,
        makeContext({ filePath: "sample.ts" }),
      ),
    );
    const zoom = renderToString(
      renderZoomResult(
        makeResult("", {
          name: "run",
          kind: "function",
          range: { start_line: 1, end_line: 4 },
          content: "export function run() {\n  return helper();\n}",
          annotations: {
            calls_out: [{ name: "helper", line: 2 }],
            called_by: [{ name: "main", line: 8 }],
          },
        }),
        { filePath: "sample.ts", symbol: "run" },
        mockTheme,
        makeContext({ filePath: "sample.ts", symbol: "run" }),
      ),
    );

    expect(outline).toContain("sample.ts");
    expect(outline).toContain("Service");
    expect(zoom).toContain("run [function]");
    expect(zoom).toContain("helper:2");
    expect(zoom).toContain("main:8");
  });

  test("reading renderers handle error and empty payloads", () => {
    const error = renderToString(
      renderZoomResult(
        makeResult("symbol not found"),
        { filePath: "sample.ts", symbol: "run" },
        mockTheme,
        makeContext({ filePath: "sample.ts", symbol: "run" }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderOutlineResult(makeResult(""), mockTheme, makeContext({ directory: "." })),
    );

    expect(error).toContain("symbol not found");
    expect(empty).toContain("No outline available");
  });

  test("batched zoom keeps successes visible when another symbol fails", () => {
    const batch = formatZoomBatchResult(
      "sample.ts",
      ["run", "Missing"],
      [
        {
          success: true,
          name: "run",
          kind: "function",
          range: { start_line: 1, end_line: 1, start_col: 0, end_col: 24 },
          content: "export function run() {}",
          context_before: [],
          context_after: [],
          annotations: { calls_out: [], called_by: [] },
        },
        { success: false, message: "symbol not found" },
      ],
    );
    const rendered = renderToString(
      renderZoomResult(
        makeResult(batch.text, batch),
        { filePath: "sample.ts", symbols: ["run", "Missing"] },
        mockTheme,
        makeContext({ filePath: "sample.ts", symbols: ["run", "Missing"] }),
      ),
    );

    expect(batch.complete).toBe(false);
    expect(batch.text).toContain("Incomplete zoom results");
    // Successful entries are now formatted as plain text via formatZoomText —
    // line-numbered, no JSON escapes.
    expect(batch.text).toContain("sample.ts:1-1 [function run]");
    expect(batch.text).toContain("export function run() {}");
    expect(batch.text).toContain('Symbol "Missing" not found: symbol not found');
    expect(rendered).toContain("Incomplete zoom results");
    expect(rendered).toContain("export function run() {}");
    expect(rendered).toContain("symbol not found");
  });
});
