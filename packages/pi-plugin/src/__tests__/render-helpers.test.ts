/**
 * Unit tests for shared renderer helper utilities.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import type { Component } from "@earendil-works/pi-tui";
import {
  collapsibleResult,
  collectTextContent,
  extractStructuredPayload,
  formatLineRange,
  formatTimestamp,
  formatUnifiedDiffForPi,
  formatValue,
  groupByFile,
  joinNonEmpty,
  normalizeTerminalText,
  severityBadge,
} from "../tools/render-helpers.js";
import { makeContext, mockTheme } from "./render-test-helpers.js";

describe("render helper utilities", () => {
  test("extractStructuredPayload prefers details and safely parses text fallback", () => {
    expect(
      extractStructuredPayload({
        content: [{ type: "text", text: '{"from":"text"}' }],
        details: { from: "details" },
      }),
    ).toEqual({ from: "details" });
    expect(extractStructuredPayload({ content: [{ type: "text", text: '{"ok":true}' }] })).toEqual({
      ok: true,
    });
    expect(
      extractStructuredPayload({ content: [{ type: "text", text: "not json" }] }),
    ).toBeUndefined();
  });

  test("normalizes carriage returns at the shared result seam", () => {
    const component = (lines: string[]): Component =>
      ({
        render: () => lines,
        invalidate: () => {},
      }) as Component;

    const expanded = collapsibleResult({
      summary: "edited file (+1/-1)",
      full: component(["- 1 old\r", "+ 1 new\r"]),
      expanded: true,
      context: makeContext({}),
    });
    const expandedLines = expanded.render(200);
    expect(expandedLines).toEqual(["- 1 old", "+ 1 new"]);
    expect(expandedLines.join("\n")).not.toContain("\r");

    const collapsed = collapsibleResult({
      summary: "edited file (+1/-1)\r",
      full: component(Array.from({ length: 6 }, (_, index) => `line ${index}\r`)),
      expanded: false,
      context: makeContext({}),
    });
    const collapsedText = collapsed.render(200).join("\n");
    expect(collapsedText).toContain("edited file (+1/-1)");
    expect(collapsedText).not.toContain("\r");
    expect(normalizeTerminalText("a\rb")).toBe("ab");
  });

  test("collectTextContent ignores non-text blocks and trims model-facing text", () => {
    const result = {
      content: [
        { type: "text", text: " first " },
        { type: "image", data: "ignored" },
        { type: "text", text: "second" },
      ],
    } as never;

    expect(collectTextContent(result)).toBe("first \nsecond");
  });

  test("formatUnifiedDiffForPi handles multi-hunk diffs and no-newline markers", () => {
    const diff = [
      "--- a/file.ts",
      "+++ b/file.ts",
      "@@ -1,2 +1,2 @@",
      " line one",
      "-old",
      "+new",
      "\\ No newline at end of file",
      "@@ -10,2 +10,2 @@",
      " context",
      "-old tail",
      "+new tail",
    ].join("\n");

    expect(formatUnifiedDiffForPi(diff)).toBe(
      ["  1 line one", "- 2 old", "+ 2 new", " 10 context", "-11 old tail", "+11 new tail"].join(
        "\n",
      ),
    );
  });

  test("diff fallback colors numbered rows and removes carriage returns without host renderDiff", () => {
    const renderer = new URL("../tools/render-helpers.ts", import.meta.url).href;
    const diff = "  9 context\r\n-10 old\r\n+24 new\r";
    // Keep the host-module mock in a child process so it cannot affect other tests.
    const source = `
      import { mock } from "bun:test";
      mock.module("@earendil-works/pi-coding-agent", () => ({}));
      const { renderNumberedDiff } = await import(${JSON.stringify(renderer)});
      const theme = { fg: (color, text) => "[" + color + "]" + text };
      process.stdout.write(renderNumberedDiff(${JSON.stringify(diff)}, theme));
    `;
    const output = execFileSync(process.execPath, ["-e", source], {
      cwd: import.meta.dir,
      encoding: "utf8",
      timeout: 5_000,
    });
    expect(output).toBe(
      "[toolDiffContext]  9 context\n[toolDiffRemoved]-10 old\n[toolDiffAdded]+24 new",
    );
  });

  test("small formatting helpers avoid undefined text leaks", () => {
    expect(joinNonEmpty(["a", undefined, "", "b"], " / ")).toBe("a / b");
    expect(formatValue(null)).toBe("—");
    expect(formatValue({ a: 1 })).toBe('{"a":1}');
    expect(formatLineRange(4)).toBe("4");
    expect(formatLineRange(4, 9)).toBe("4-9");
    expect(formatTimestamp(1_700_000_000)).toContain("2023-11-14");
    expect(severityBadge(mockTheme, "information")).toContain("[info]");
  });

  test("groupByFile falls back to an explicit unknown-file bucket", () => {
    const groups = groupByFile(
      [{ file: "a.ts" }, { file: undefined }, { file: "a.ts" }],
      (item) => item.file,
    );

    expect(groups.get("a.ts")).toHaveLength(2);
    expect(groups.get("(unknown file)")).toHaveLength(1);
  });
});
