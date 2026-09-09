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
    expect(output).toContain("search");
    expect(output).toContain("find auth logic");
  });

  test("renderSemanticResult groups ready results by file", () => {
    const output = renderToString(
      renderSemanticResult(
        makeResult("", {
          status: "ready",
          semantic_status: "ready",
          interpreted_as: "hybrid",
          query_kind: "Identifier",
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

    expect(output).toContain("semantic: ready");
    expect(output).toContain("src/auth.ts");
    expect(output).toContain("login [function] lines 4-8 score 0.910");
  });

  test("renderSemanticResult surfaces semantic honesty flags", () => {
    const output = renderToString(
      renderSemanticResult(
        makeResult("", {
          status: "ready",
          semantic_status: "ready",
          interpreted_as: "hybrid",
          more_available: true,
          engine_capped: true,
          fully_degraded: true,
          complete: false,
          results: [
            {
              file: "/repo/src/auth.ts",
              name: "login",
              kind: "function",
              start_line: 4,
              end_line: 8,
            },
          ],
        }),
        { query: "auth", topK: 5 },
        mockTheme,
        makeContext({ query: "auth", topK: 5 }),
      ),
    );

    expect(output).toContain(
      "Search status: more results available; enumeration capped; fully degraded; partial/incomplete.",
    );
  });

  test("renderSemanticResult renders file_summary results as summaries", () => {
    const output = renderToString(
      renderSemanticResult(
        makeResult("", {
          status: "ready",
          semantic_status: "ready",
          interpreted_as: "semantic",
          results: [
            {
              file: "/repo/src/auth.ts",
              name: "auth.ts",
              kind: "file_summary",
              start_line: null,
              end_line: null,
              location: "[file summary]",
              score: 0.82,
              source: "semantic",
              snippet: "Exports login and session helpers.",
            },
          ],
        }),
        { query: "auth", topK: 5 },
        mockTheme,
        makeContext({ query: "auth", topK: 5 }),
      ),
    );

    expect(output).toContain("src/auth.ts");
    expect(output).toContain("Exports login and session helpers.");
    expect(output).toContain("[file summary score 0.820]");
    expect(output).not.toContain("lines ?");
  });

  test("renderSemanticResult renders lexical file_summary results as lexical matches", () => {
    const output = renderToString(
      renderSemanticResult(
        makeResult("", {
          status: "ready",
          semantic_status: "ready",
          interpreted_as: "hybrid",
          results: [
            {
              file: "/repo/src/auth.ts",
              name: "auth.ts",
              kind: "file_summary",
              start_line: null,
              end_line: null,
              location: "[lexical match]",
              score: 0.77,
              source: "lexical",
              snippet: "Exports login and session helpers.",
            },
          ],
        }),
        { query: "login", topK: 5 },
        mockTheme,
        makeContext({ query: "login", topK: 5 }),
      ),
    );

    expect(output).toContain("src/auth.ts");
    expect(output).toContain("[lexical match — score 0.770]");
    expect(output).toContain("Exports login and session helpers.");
    expect(output).not.toContain("[file summary");
  });

  test("renderSemanticResult renders GrepLine results", () => {
    const output = renderToString(
      renderSemanticResult(
        makeResult("", {
          status: "ready",
          semantic_status: "disabled",
          interpreted_as: "regex",
          query_kind: "Regex",
          results: [
            {
              kind: "GrepLine",
              file: "/repo/src/auth.ts",
              line: 12,
              column: 5,
              line_text: "export function login() {}",
            },
          ],
        }),
        { query: ".*login", topK: 5 },
        mockTheme,
        makeContext({ query: ".*login", topK: 5 }),
      ),
    );

    expect(output).toContain("mode=regex");
    expect(output).toContain("src/auth.ts");
    expect(output).toContain("line 12:5 export function login() {}");
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

    expect(building).toContain("semantic: building");
    expect(error).toContain("embedding failed");
  });
});
