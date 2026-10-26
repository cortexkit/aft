/**
 * Unit tests for AST tool_call argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import { registerAstTools } from "../tools/ast.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("AST tool adapters", () => {
  test("SystemVerilog schemas accept both names and preserve HDL replacements", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "preview" }));
    registerAstTools(api, makePluginContext(bridge), { astSearch: true, astReplace: true });
    const search = tools.get("ast_grep_search")!;
    const replace = tools.get("ast_grep_replace")!;
    for (const lang of ["systemverilog", "verilog"]) {
      const args = {
        pattern: '$display("value=%d", $VALUE)',
        rewrite: '$display("next=%d", $VALUE)',
        lang,
        dryRun: true,
      };
      expect(Value.Check(search.parameters as TSchema, { pattern: args.pattern, lang })).toBe(true);
      expect(Value.Check(replace.parameters as TSchema, args)).toBe(true);
      await executeTool(replace, args, makeExtContext(projectRoot));
      expect(calls.at(-1)?.params).toMatchObject({ name: "ast_replace", arguments: args });
    }
  });

  test("ast_grep_search forwards agent-facing contextLines and preserves globs", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "Found 0 matches" }));
    registerAstTools(api, makePluginContext(bridge), { astSearch: true, astReplace: true });

    await executeTool(
      tools.get("ast_grep_search")!,
      {
        pattern: "console.log($MSG)",
        lang: "typescript",
        paths: ["src"],
        globs: ["**/*.ts", "!dist/**"],
        contextLines: 3,
      },
      makeExtContext(projectRoot),
    );

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toEqual({
      name: "ast_search",
      arguments: {
        pattern: "console.log($MSG)",
        lang: "typescript",
        paths: ["src"],
        globs: ["**/*.ts", "!dist/**"],
        contextLines: 3,
      },
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

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toMatchObject({
      name: "ast_replace",
      arguments: {
        pattern: "console.log($MSG)",
        rewrite: "logger.info($MSG)",
        lang: "typescript",
        dryRun: false,
      },
    });
  });

  test("ast_grep_replace preserves explicit dryRun previews", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "preview" }));
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
      name: "ast_replace",
      arguments: {
        paths: ["lib"],
        globs: ["**/*.js"],
        dryRun: true,
      },
    });
  });

  test('ast_grep_replace treats string dryRun "true" as preview (dry_run true)', async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "preview" }));
    registerAstTools(api, makePluginContext(bridge), { astSearch: false, astReplace: true });

    await executeTool(tools.get("ast_grep_replace")!, {
      pattern: "foo($A)",
      rewrite: "bar($A)",
      lang: "javascript",
      dryRun: "true" as unknown as boolean,
    });

    expect(calls[0].command).toBe("tool_call");
    expect((calls[0].params.arguments as Record<string, unknown>).dryRun).toBe(true);
  });
});
