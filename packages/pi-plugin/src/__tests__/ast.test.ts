/**
 * Unit tests for AST tool argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerAstTools } from "../tools/ast.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("AST tool adapters", () => {
  test("ast_grep_search maps contextLines to context_lines and preserves globs", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "Found 0 matches" }));
    registerAstTools(api, makePluginContext(bridge), { astSearch: true, astReplace: true });

    await executeTool(tools.get("ast_grep_search")!, {
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: ["src"],
      globs: ["**/*.ts", "!dist/**"],
      contextLines: 3,
    });

    expect(calls[0].command).toBe("ast_search");
    expect(calls[0].params).toEqual({
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: ["src"],
      globs: ["**/*.ts", "!dist/**"],
      context_lines: 3,
    });
  });

  test("ast_grep_replace applies by default instead of inheriting Rust dry-run default", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "Replaced 1 match" }));
    registerAstTools(api, makePluginContext(bridge), { astSearch: true, astReplace: true });

    await executeTool(tools.get("ast_grep_replace")!, {
      pattern: "console.log($MSG)",
      rewrite: "logger.info($MSG)",
      lang: "typescript",
    });

    expect(calls[0].command).toBe("ast_replace");
    expect(calls[0].params).toMatchObject({
      pattern: "console.log($MSG)",
      rewrite: "logger.info($MSG)",
      lang: "typescript",
      dry_run: false,
    });
  });

  test("ast_grep_replace preserves explicit dryRun previews", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerAstTools(api, makePluginContext(bridge), { astSearch: false, astReplace: true });

    await executeTool(tools.get("ast_grep_replace")!, {
      pattern: "foo($A)",
      rewrite: "bar($A)",
      lang: "javascript",
      dryRun: true,
      paths: ["lib"],
      globs: ["**/*.js"],
    });

    expect(calls[0].params).toMatchObject({
      paths: ["lib"],
      globs: ["**/*.js"],
      dry_run: true,
    });
  });
});
