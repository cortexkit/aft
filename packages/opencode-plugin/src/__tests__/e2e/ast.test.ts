/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { astTools } from "../../tools/ast.js";
import type { PluginContext } from "../../types.js";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

maybeDescribe("e2e ast commands", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary);
    harnesses.push(created);
    return created;
  }

  test("ast_search finds a pattern with locations", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: [h.path("multi-match.ts")],
    });

    expect(response.success).toBe(true);
    expect(response.total_matches).toBe(5);
    expect(response.files_with_matches).toBe(1);
  });

  test("ast_search captures meta-variables", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "export const $NAME = $VALUE",
      lang: "typescript",
      paths: [h.path("sample.ts")],
    });

    expect(response.success).toBe(true);
    const matches = response.matches as Array<Record<string, unknown>>;
    expect(matches.length).toBeGreaterThan(0);
    expect((matches[0]?.meta_variables as Record<string, unknown>)?.$NAME).toBe("DEFAULT_SUFFIX");
    expect((matches[0]?.meta_variables as Record<string, unknown>)?.$VALUE).toBe('"!"');
  });

  test("ast_search reports clean empty results", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: [h.path("sample.ts")],
    });

    expect(response.success).toBe(true);
    expect(response.total_matches).toBe(0);
  });

  test("ast_search rejects invalid patterns without crashing", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_replace", {
      pattern: "catch ($ERR) { $$$ }",
      rewrite: "noop()",
      lang: "typescript",
      dry_run: true,
    });

    expect(response.success).toBe(false);
    expect(response.code).toBe("invalid_pattern");
  });

  test("ast_replace updates a single file", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");

    const response = await h.bridge.send("ast_replace", {
      pattern: "console.log($MSG)",
      rewrite: "logger.info($MSG)",
      lang: "typescript",
      paths: [filePath],
      dry_run: false,
    });

    expect(response.success).toBe(true);
    expect(response.total_replacements).toBe(5);
    const content = await readTextFile(filePath);
    expect(content.match(/logger\.info/g)?.length).toBe(5);
  });

  test("ast_replace dry run leaves files unchanged", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");
    const original = await readTextFile(filePath);

    const response = await h.bridge.send("ast_replace", {
      pattern: "console.log($MSG)",
      rewrite: "logger.debug($MSG)",
      lang: "typescript",
      paths: [filePath],
      dry_run: true,
    });

    expect(response.success).toBe(true);
    expect(response.dry_run).toBe(true);
    expect(await readTextFile(filePath)).toBe(original);
  });

  test("ast_replace preserves meta-variables in rewrite", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");

    const response = await h.bridge.send("ast_replace", {
      pattern: "console.log($MSG)",
      rewrite: "report($MSG)",
      lang: "typescript",
      paths: [filePath],
      dry_run: false,
    });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    expect(content).toContain('report("alpha")');
    expect(content).toContain('report("epsilon")');
  });

  test("ast_search works for python fixtures too", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "return $VALUE",
      lang: "python",
      paths: [h.path("sample.py")],
    });

    expect(response.success).toBe(true);
    expect(Number(response.total_matches)).toBeGreaterThanOrEqual(4);
  });

  test("ast_grep_search uses tool_call formatting for matches, hints, and empty scopes", async () => {
    const h = await harness();
    const search = astTools(pluginContext(h)).ast_grep_search;

    const found = await search.execute(
      {
        pattern: "export const $NAME = $VALUE",
        lang: "typescript",
        paths: ["sample.ts"],
      },
      runtime(h),
    );
    expect(found).toContain("Found 1 match(es) in 1 file(s)");
    expect(found).toContain("$NAME: DEFAULT_SUFFIX");
    expect(found).toContain('$VALUE: "!"');

    const zero = await search.execute(
      { pattern: "noEmit|NoEmit", lang: "typescript", paths: ["sample.ts"] },
      runtime(h),
    );
    expect(zero).toContain("No matches found");
    expect(zero).toContain('Hint: "|" does NOT mean alternation');

    await mkdir(h.path("empty"));
    const scopeZero = await search.execute(
      { pattern: "console.log($MSG)", lang: "typescript", paths: ["empty"] },
      runtime(h),
    );
    expect(scopeZero).toContain("No files matched the scope");
    expect(scopeZero).toContain("Scope warnings:");
  });

  test("ast_grep_replace uses tool_call for dry-run and apply behavior", async () => {
    const h = await harness();
    const replace = astTools(pluginContext(h)).ast_grep_replace;
    const filePath = h.path("multi-match.ts");
    const original = await readTextFile(filePath);

    const preview = await replace.execute(
      {
        pattern: "console.log($MSG)",
        rewrite: "logger.debug($MSG)",
        lang: "typescript",
        paths: ["multi-match.ts"],
        dryRun: true,
      },
      runtime(h),
    );
    expect(preview).toContain("[DRY RUN] Would replace 5 match(es)");
    expect(preview).toContain("logger.debug");
    expect(await readTextFile(filePath)).toBe(original);

    const applied = await replace.execute(
      {
        pattern: "console.log($MSG)",
        rewrite: "logger.info($MSG)",
        lang: "typescript",
        paths: ["multi-match.ts"],
        dryRun: false,
      },
      runtime(h),
    );
    expect(applied).toContain("Replaced 5 match(es)");
    expect(await readTextFile(filePath)).toContain("logger.info");
  });

  test("ast_grep_replace asks edit permission only for apply mode", async () => {
    const h = await harness();
    const replace = astTools(pluginContext(h)).ast_grep_replace;
    const external = mkdtempSync(join(dirname(h.tempDir), "aft-ast-external-"));
    try {
      const externalFile = join(external, "external.ts");
      await writeFile(
        externalFile,
        "export function run() {\n  console.log('external');\n}\n",
        "utf8",
      );

      const applyAsks: AskCall[] = [];
      await replace.execute(
        {
          pattern: "console.log($MSG)",
          rewrite: "logger.info($MSG)",
          lang: "typescript",
          paths: [external],
          dryRun: false,
        },
        runtime(h, recordingAsk(applyAsks)),
      );
      expect(applyAsks.some((call) => call.permission === "external_directory")).toBe(true);
      expect(applyAsks.some((call) => call.permission === "edit")).toBe(true);
      expect(await readTextFile(externalFile)).toContain("logger.info");

      await writeFile(
        externalFile,
        "export function run() {\n  console.log('external');\n}\n",
        "utf8",
      );
      const dryRunAsks: AskCall[] = [];
      const preview = await replace.execute(
        {
          pattern: "console.log($MSG)",
          rewrite: "logger.debug($MSG)",
          lang: "typescript",
          paths: [external],
          dryRun: true,
        },
        runtime(h, recordingAsk(dryRunAsks)),
      );
      expect(preview).toContain("[DRY RUN]");
      expect(dryRunAsks.some((call) => call.permission === "external_directory")).toBe(true);
      expect(dryRunAsks.some((call) => call.permission === "edit")).toBe(false);
      expect(await readTextFile(externalFile)).toContain("console.log");
    } finally {
      rmSync(external, { recursive: true, force: true });
    }
  });
});

function pluginContext(harness: E2EHarness): PluginContext {
  const pool = {
    getBridge: () => harness.bridge,
  } as unknown as BridgePool;
  return {
    pool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: {
      disabled_tools: [],
      lsp: { diagnostics_on_edit: false },
    } as PluginContext["config"],
    storageDir: harness.path(".storage"),
  };
}

function runtime(
  harness: E2EHarness,
  ask: ToolContext["ask"] = async () => undefined,
): Parameters<ToolDefinition["execute"]>[1] {
  return {
    directory: harness.tempDir,
    worktree: harness.tempDir,
    sessionID: undefined,
    messageID: "ast-toolcall-e2e",
    agent: "test",
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  } as unknown as Parameters<ToolDefinition["execute"]>[1];
}

function recordingAsk(calls: AskCall[]): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}
