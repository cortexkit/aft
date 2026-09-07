/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { inspectTools } from "../../tools/inspect.js";
import { readingTools } from "../../tools/reading.js";
import { searchTools } from "../../tools/search.js";
import { semanticTools } from "../../tools/semantic.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function createMockClient(): PluginContext["client"] {
  return { lsp: {}, find: {} } as PluginContext["client"];
}

function createPluginContext(harness: E2EHarness): PluginContext {
  const pool = { getBridge: () => harness.bridge } as unknown as BridgePool;
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir: harness.path(".aft-test-storage"),
  };
}

function createToolContext(harness: E2EHarness): ToolContext {
  return {
    sessionID: "read-only-spine-toolcall-e2e",
    messageID: "read-only-spine-toolcall-message",
    agent: "test",
    directory: harness.tempDir,
    worktree: harness.tempDir,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  } as ToolContext;
}

async function createFixtureProject(harness: E2EHarness): Promise<void> {
  await mkdir(harness.path("src"), { recursive: true });
  await Promise.all([
    writeFile(
      harness.path("src", "hit.ts"),
      [
        "export const toolCallGrepMarker = 'tool_call_grep_marker';",
        "export const toolCallSearchMarker = 'tool_call_search_marker';",
        "export function toolCallOutlineFunction(input: string): string {",
        "  return input;",
        "}",
        "export class ToolCallOutlineService {",
        "  run(): void {}",
        "}",
        "// TODO cutover inspect marker",
        "",
      ].join("\n"),
      "utf8",
    ),
    writeFile(
      harness.path("src", "other.ts"),
      "export function toolCallOutlineOther(): boolean { return true; }\n",
      "utf8",
    ),
    writeFile(
      harness.path("src", "hit.test.ts"),
      "export function toolCallOutlineTestOnly(): void {}\n",
      "utf8",
    ),
  ]);
}

export function runReadOnlySpineToolcallSuite(
  options: { harnessFactory?: HarnessFactory; name?: string } = {},
): void {
  maybeDescribe(options.name ?? "e2e read-only spine tool_call cutover", () => {
    let preparedBinary: PreparedBinary = initialBinary;
    const harnesses: E2EHarness[] = [];

    beforeAll(async () => {
      preparedBinary = await prepareBinary();
    });

    afterEach(async () => {
      await cleanupHarnesses(harnesses);
    });

    async function harness(): Promise<E2EHarness> {
      const created = await (options.harnessFactory ?? createHarness)(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
        tempPrefix: "aft-plugin-readonly-spine-",
      });
      harnesses.push(created);
      await createFixtureProject(created);
      return created;
    }

    test("grep returns server-rendered matches through tool_call", async () => {
      const h = await harness();
      const tools = searchTools(createPluginContext(h));

      const output = await tools.grep.execute(
        { pattern: "tool_call_grep_marker", path: "src" },
        createToolContext(h),
      );

      expect(output).toContain("tool_call_grep_marker");
      expect(output).toContain("Found 1 match across 1 file");
    });

    test("grep appends the plugin-side skipped path footer after tool_call", async () => {
      const h = await harness();
      const tools = searchTools(createPluginContext(h));

      const output = await tools.grep.execute(
        { pattern: "tool_call_grep_marker", path: "src missing-dir" },
        createToolContext(h),
      );

      expect(output).toContain("tool_call_grep_marker");
      expect(output).toContain("Found 1 match across 1 file");
      expect(output).toContain("Skipped 1 path not found: missing-dir");
    });

    test("aft_search returns server-rendered literal search text through tool_call", async () => {
      const h = await harness();
      const tools = semanticTools(createPluginContext(h));

      const output = await tools.aft_search.execute(
        { query: "tool_call_search_marker", hint: "literal", topK: 5 },
        createToolContext(h),
      );

      expect(output).toContain("tool_call_search_marker");
      expect(output).toContain("Found ");
    });

    test("aft_inspect returns server-rendered todos details through tool_call", async () => {
      const h = await harness();
      const tools = inspectTools(createPluginContext(h));

      const output = await tools.aft_inspect.execute(
        { sections: "todos", topK: 5 },
        createToolContext(h),
      );

      expect(output).toContain("FRESH");
      expect(output).toContain("wait-stamp:");
      expect(output).toContain("completed phases:");
      expect(output).toContain("TODOs: 1");
      expect(output).toContain("src/hit.ts:9 TODO cutover inspect marker");
    }, 90_000);

    test("aft_outline returns single-file Text output through tool_call", async () => {
      const h = await harness();
      const tools = readingTools(createPluginContext(h));

      const output = await tools.aft_outline.execute(
        { target: "src/hit.ts" },
        createToolContext(h),
      );

      expect(output).toContain("hit.ts");
      expect(output).toContain("toolCallOutlineFunction");
      expect(output).toContain("ToolCallOutlineService");
    });

    test("aft_outline returns plain-text directory outline through tool_call", async () => {
      const h = await harness();
      const tools = readingTools(createPluginContext(h));

      const output = await tools.aft_outline.execute({ target: "src" }, createToolContext(h));

      // Directory outlines render as plain text like every other outline
      // mode; a complete walk carries no partial-result footer.
      expect(() => JSON.parse(output)).toThrow();
      expect(output).toContain("src/");
      expect(output).toContain("hit.ts");
      expect(output).toContain("toolCallOutlineFunction");
      expect(output).not.toContain("hit.test.ts");
      expect(output).not.toContain("⚠ Partial result");
    });

    test("aft_outline files:true returns the server-rendered files tree through tool_call", async () => {
      const h = await harness();
      const tools = readingTools(createPluginContext(h));

      const output = await tools.aft_outline.execute(
        { target: "src", files: true },
        createToolContext(h),
      );

      expect(output).toContain("typescript");
      expect(output).toContain("hit.ts");
      expect(output).toContain("other.ts");
      // includeTests defaults to false in files mode as in every other mode;
      // the includeTests test below covers the opt-in.
      expect(output).not.toContain("hit.test.ts");
    });

    test("aft_outline returns multi-file Text output for array targets through tool_call", async () => {
      const h = await harness();
      const tools = readingTools(createPluginContext(h));

      const output = await tools.aft_outline.execute(
        { target: ["src/hit.ts", "src/other.ts"] },
        createToolContext(h),
      );

      expect(output).toContain("src/");
      expect(output).toContain("hit.ts");
      expect(output).toContain("toolCallOutlineFunction");
      expect(output).toContain("other.ts");
      expect(output).toContain("toolCallOutlineOther");
    });

    test("aft_outline includeTests controls directory test-file visibility through tool_call", async () => {
      const h = await harness();
      const tools = readingTools(createPluginContext(h));

      const withoutTests = await tools.aft_outline.execute({ target: "src" }, createToolContext(h));
      const withTests = await tools.aft_outline.execute(
        { target: "src", includeTests: true },
        createToolContext(h),
      );

      expect(withoutTests).not.toContain("hit.test.ts");
      expect(withoutTests).not.toContain("toolCallOutlineTestOnly");
      expect(withTests).toContain("hit.test.ts");
      expect(withTests).toContain("toolCallOutlineTestOnly");
    });
  });
}

if (process.env.AFT_OPENCODE_E2E_IMPORT_ONLY !== "1") {
  runReadOnlySpineToolcallSuite();
}
