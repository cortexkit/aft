/**
 * Renderer coverage for aft_callgraph.
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
    expect(output).toContain("callgraph");
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
          depth_limited: true,
          truncated: 2,
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
          depth_limited: true,
          truncated: 3,
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
          root_counts: { public_api: 1 },
          max_depth_reached: true,
          truncated_paths: 4,
          paths: [
            {
              root: "public_api",
              hops: [
                { symbol: "main", file: "/repo/src/a.ts", line: 1, exported: true },
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
          depth_limited: true,
          truncated: 5,
          callers: [
            {
              caller_symbol: "main",
              caller_file: "/repo/src/a.ts",
              line: 7,
              call_expression: "run()",
              parameters: ["x"],
              root: "public_api",
            },
          ],
          root_counts: { public_api: 1 },
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
    expect(callTree).toContain("2 truncated");
    expect(callers).toContain("caller");
    expect(callers).toContain("3 truncated");
    expect(traceTo).toContain("Path 1");
    expect(traceTo).toContain("4 truncated");
    expect(traceTo).toContain("roots: 1 public API function");
    expect(traceTo).toContain("main [root: public API]");
    expect(impact).toContain("affected call site");
    expect(impact).toContain("5 truncated");
    expect(impact).toContain("Reaches 1 public API function");
    expect(impact).toContain("main [root: public API]");
    expect(traceData).toContain("depth limited");
  });

  test("renderNavigateResult honors includeUnresolved", () => {
    const payload = {
      name: "run",
      file: "/repo/src/a.ts",
      line: 1,
      children: [
        { name: "len", file: "/repo/src/a.ts", line: 2, resolved: false, children: [] },
        { name: "Some", file: "/repo/src/a.ts", line: 3, resolved: false, children: [] },
      ],
    };

    const collapsed = renderToString(
      renderNavigateResult(
        makeResult("", payload),
        { op: "call_tree", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "call_tree", filePath: "src/a.ts", symbol: "run" }),
      ),
    );
    const expanded = renderToString(
      renderNavigateResult(
        makeResult("", payload),
        { op: "call_tree", filePath: "src/a.ts", symbol: "run", includeUnresolved: true },
        mockTheme,
        makeContext({
          op: "call_tree",
          filePath: "src/a.ts",
          symbol: "run",
          includeUnresolved: true,
        }),
      ),
    );

    expect(collapsed).toContain("+ 2 unresolved external calls: len, Some");
    expect(collapsed).not.toContain("len [/repo/src/a.ts:2] [unresolved]");
    expect(expanded).toContain("len [/repo/src/a.ts:2] [unresolved]");
    expect(expanded).not.toContain("unresolved external calls");
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
        makeResult("", { total_paths: 0, paths: [] }),
        { op: "trace_to", filePath: "src/a.ts", symbol: "run" },
        mockTheme,
        makeContext({ op: "trace_to", filePath: "src/a.ts", symbol: "run" }),
      ),
    );

    expect(error).toContain("not configured");
    expect(empty).toContain("No paths found");
  });
});
