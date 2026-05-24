/**
 * Renderer coverage for aft_safety.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderSafetyCall, renderSafetyResult } from "../tools/safety.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("safety renderer", () => {
  test("renderSafetyCall shows op and target", () => {
    const output = renderToString(
      renderSafetyCall(
        { op: "undo", filePath: "src/a.ts" },
        mockTheme,
        makeContext({ op: "undo", filePath: "src/a.ts" }),
      ),
    );
    expect(output).toContain("safety");
    expect(output).toContain("undo");
    expect(output).toContain("src/a.ts");
  });

  test("renderSafetyResult formats undo/history/checkpoint/list", () => {
    const undo = renderToString(
      renderSafetyResult(
        makeResult("", { path: "src/a.ts", backup_id: "b1" }),
        { op: "undo", filePath: "src/a.ts" },
        mockTheme,
        makeContext({ op: "undo", filePath: "src/a.ts" }),
      ),
    );
    const history = renderToString(
      renderSafetyResult(
        makeResult("", {
          file: "src/a.ts",
          entries: [{ backup_id: "b1", timestamp: 1_700_000_000, description: "pre-edit" }],
        }),
        { op: "history", filePath: "src/a.ts" },
        mockTheme,
        makeContext({ op: "history", filePath: "src/a.ts" }),
      ),
    );
    const checkpoint = renderToString(
      renderSafetyResult(
        makeResult("", {
          name: "cp1",
          file_count: 2,
          skipped: [{ file: "missing.ts", error: "not found" }],
        }),
        { op: "checkpoint", name: "cp1" },
        mockTheme,
        makeContext({ op: "checkpoint", name: "cp1" }),
      ),
    );
    const list = renderToString(
      renderSafetyResult(
        makeResult("", {
          checkpoints: [{ name: "cp1", file_count: 2, created_at: 1_700_000_000 }],
        }),
        { op: "list" },
        mockTheme,
        makeContext({ op: "list" }),
      ),
    );

    expect(undo).toContain("backup b1");
    expect(history).toContain("pre-edit");
    expect(checkpoint).toContain("skipped");
    expect(list).toContain("cp1");
  });

  test("renderSafetyResult handles error and empty payloads", () => {
    const error = renderToString(
      renderSafetyResult(
        makeResult("no undo history"),
        { op: "undo", filePath: "src/a.ts" },
        mockTheme,
        makeContext({ op: "undo", filePath: "src/a.ts" }, { isError: true }),
      ),
    );
    const empty = renderToString(
      renderSafetyResult(
        makeResult("", { checkpoints: [] }),
        { op: "list" },
        mockTheme,
        makeContext({ op: "list" }),
      ),
    );

    expect(error).toContain("no undo history");
    expect(empty).toContain("No checkpoints saved");
  });
});
