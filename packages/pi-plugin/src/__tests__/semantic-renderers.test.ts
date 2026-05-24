/**
 * Renderer coverage for aft_search.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { renderSemanticCall, renderSemanticResult } from "../tools/semantic.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

describe("semantic renderer", () => {
  test("renderSemanticCall shows query", () => {
    const output = renderToString(
      renderSemanticCall(
        { query: "find auth logic", topK: 5 },
        mockTheme,
        makeContext({ query: "find auth logic", topK: 5 }),
      ),
    );
    expect(output).toContain("semantic search");
    expect(output).toContain("find auth logic");
  });

  test("renderSemanticResult groups ready results by file", () => {
    const output = renderToString(
      renderSemanticResult(
        makeResult("", {
          status: "ready",
          results: [
            {
              file: "/repo/src/auth.ts",
              name: "login",
              kind: "function",
              start_line: 4,
              end_line: 8,
              score: 0.91,
              snippet: "export function login() {}",
            },
          ],
        }),
        { query: "find auth logic", topK: 5 },
        mockTheme,
        makeContext({ query: "find auth logic", topK: 5 }),
      ),
    );

    expect(output).toContain("index: ready");
    expect(output).toContain("src/auth.ts");
    expect(output).toContain("login [function] lines 4-8 score 0.910");
  });

  test("renderSemanticResult handles non-ready, error, and empty payloads", () => {
    const building = renderToString(
      renderSemanticResult(
        makeResult("", { status: "building", text: "Semantic index is still building." }),
        { query: "find auth logic", topK: 5 },
        mockTheme,
        makeContext({ query: "find auth logic", topK: 5 }),
      ),
    );
    const error = renderToString(
      renderSemanticResult(
        makeResult("embedding failed"),
        { query: "find auth logic", topK: 5 },
        mockTheme,
        makeContext({ query: "find auth logic", topK: 5 }, { isError: true }),
      ),
    );

    expect(building).toContain("index: building");
    expect(error).toContain("embedding failed");
  });
});
