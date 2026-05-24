/**
 * Renderer coverage for aft_navigate.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderNavigateCall, renderNavigateResult } from "../tools/navigate.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("navigate renderer", () => {
  test("renderNavigateCall shows op and symbol", () => {
    const output = renderToString(
      renderNavigateCall(
        { op: "call_tree", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "call_tree", filePath: "src/a.ts", symbol: "run" }),
      ),
    );
    expect(output).toContain("navigate");
    expect(output).toContain("call_tree");
    expect(output).toContain("run");
  });

  test("renderNavigateResult formats all op variants", () => {
    const callTree = renderToString(
      renderNavigateResult(
        makeResult("", {
          name: "run",
          file: "/repo/src/a.ts",
          line: 1,
          children: [{ name: "helper", file: "/repo/src/a.ts", line: 4, children: [] }],
        }),
        { op: "call_tree", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "call_tree", filePath: "src/a.ts", symbol: "run" }),
      ),
    );
    const callers = renderToString(
      renderNavigateResult(
        makeResult("", {
          total_callers: 1,
          callers: [{ file: "/repo/src/a.ts", callers: [{ symbol: "caller", line: 9 }] }],
        }),
        { op: "callers", filePath: "src/a.ts", symbol: "helper" },
        mockTheme,
        makeContext({ op: "callers", filePath: "src/a.ts", symbol: "helper" }),
      ),
    );
    const traceTo = renderToString(
      renderNavigateResult(
        makeResult("", {
          total_paths: 1,
          entry_points_found: 1,
          paths: [
            {
              hops: [
                { symbol: "main", file: "/repo/src/a.ts", line: 1, is_entry_point: true },
                { symbol: "run", file: "/repo/src/a.ts", line: 4 },
              ],
            },
          ],
        }),
        { op: "trace_to", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "trace_to", filePath: "src/a.ts", symbol: "run" }),
      ),
    );
    const impact = renderToString(
      renderNavigateResult(
        makeResult("", {
          total_affected: 1,
          affected_files: 1,
          callers: [
            {
              caller_symbol: "main",
              caller_file: "/repo/src/a.ts",
              line: 7,
              call_expression: "run()",
              parameters: ["x"],
              is_entry_point: true,
            },
          ],
        }),
        { op: "impact", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "impact", filePath: "src/a.ts", symbol: "run" }),
      ),
    );
    const traceData = renderToString(
      renderNavigateResult(
        makeResult("", {
          depth_limited: true,
          hops: [
            {
              file: "/repo/src/a.ts",
              symbol: "run",
              variable: "name",
              line: 3,
              flow_type: "parameter",
              approximate: true,
            },
          ],
        }),
        { op: "trace_data", filePath: "src/a.ts", symbol: "run", expression: "name" },
        mockTheme,
        makeContext({ op: "trace_data", filePath: "src/a.ts", symbol: "run", expression: "name" }),
      ),
    );

    expect(callTree).toContain("helper");
    expect(callers).toContain("caller");
    expect(traceTo).toContain("Path 1");
    expect(impact).toContain("affected call site");
    expect(traceData).toContain("depth limited");
  });

  test("renderNavigateResult handles error and empty payloads", () => {
    const error = renderToString(
      renderNavigateResult(
        makeResult("not configured"),
        { op: "call_tree", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "call_tree", filePath: "src/a.ts", symbol: "run" }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderNavigateResult(
        makeResult("", { total_paths: 0, entry_points_found: 0, paths: [] }),
        { op: "trace_to", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "trace_to", filePath: "src/a.ts", symbol: "run" }),
      ),
    );

    expect(error).toContain("not configured");
    expect(empty).toContain("No entry paths found");
  });
});
