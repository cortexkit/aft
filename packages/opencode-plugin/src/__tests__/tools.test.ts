/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { aftPrefixedTools } from "../tools/hoisted.js";
import { formatZoomBatchResult, readingTools } from "../tools/reading.js";
import { refactoringTools } from "../tools/refactoring.js";
import { safetyTools } from "../tools/safety.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

const BINARY_PATH = resolve(import.meta.dir, "../../../../target/debug/aft");
const PROJECT_CWD = resolve(import.meta.dir, "../../../..");
const FIXTURE_FILE = resolve(PROJECT_CWD, "crates/aft/tests/fixtures/sample.ts");
let sdkCtx = createMockSdkContext(PROJECT_CWD);
const TEST_TIMEOUT_MS = 10_000;

/**
 * Creates a mock client that returns no connected LSP servers.
 * This ensures queryLspHints returns undefined (no-op) during integration tests.
 */
function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
  };
}

/** Helper to create a PluginContext with a pool and a mock client. */
function createPluginContext(pool: BridgePool): PluginContext {
  return { pool, client: createMockClient(), config: {} as any, storageDir: "/tmp/aft-test" };
}

/** Mock SDK ToolContext for test execute calls. */
function createMockSdkContext(directory: string): ToolContext {
  return {
    sessionID: "test",
    messageID: "test",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

describe("Tool round-trips", () => {
  let pool: BridgePool;
  let tmpDir: string | null = null;

  // Fresh pool per test — each test is independent
  const createBridge = () => {
    pool = new BridgePool(
      BINARY_PATH,
      {
        timeoutMs: TEST_TIMEOUT_MS,
      },
      { harness: "opencode" },
    );
    return pool;
  };

  afterEach(async () => {
    if (pool) {
      pool.shutdown();
    }
    if (tmpDir) {
      await rm(tmpDir, { recursive: true, force: true });
      tmpDir = null;
    }
    sdkCtx = createMockSdkContext(PROJECT_CWD);
  });

  test("aft_outline tool returns tree text for fixture file with known symbols", async () => {
    createBridge();
    const tools = readingTools(createPluginContext(pool));

    const text = await tools.aft_outline.execute({ target: FIXTURE_FILE }, sdkCtx);

    // Output is now tree-formatted text, not JSON
    expect(typeof text).toBe("string");
    expect(text.length).toBeGreaterThan(0);

    // Verify known symbols appear in the tree text
    expect(text).toContain("greet");
    expect(text).toContain("add");
    expect(text).toContain("UserService");
    expect(text).toContain("Config");
    expect(text).toContain("Status");
    expect(text).toContain("UserId");
    expect(text).toContain("internalHelper");

    // Verify kind abbreviations and exported markers
    expect(text).toContain("E fn"); // exported function
    expect(text).toContain("E cls"); // exported class
    expect(text).toContain("- fn"); // internal function (internalHelper)
  });

  test("batched zoom surfaces both successes and per-symbol failures", () => {
    const batch = formatZoomBatchResult(
      "src/sample.ts",
      ["greet", "Missing"],
      [
        {
          success: true,
          name: "greet",
          kind: "function",
          range: { start_line: 1, end_line: 1, start_col: 0, end_col: 26 },
          content: "export function greet() {}",
          context_before: [],
          context_after: [],
          annotations: { calls_out: [], called_by: [] },
        },
        { success: false, message: "symbol not found" },
      ],
    );

    expect(batch.complete).toBe(false);
    expect(batch.symbols[0]?.name).toBe("greet");
    expect(batch.symbols[0]?.success).toBe(true);
    // Successful entries now contain plain-text formatted output (line-numbered,
    // not JSON-escaped) routed through formatZoomText.
    expect(batch.symbols[0]?.content).toContain("src/sample.ts:1-1 [function greet]");
    expect(batch.symbols[0]?.content).toContain("1: export function greet() {}");
    expect(batch.symbols[1]).toEqual({
      name: "Missing",
      success: false,
      error: "symbol not found",
    });
    expect(batch.text).toContain("Incomplete zoom results");
    expect(batch.text).toContain("export function greet() {}");
    expect(batch.text).toContain('Symbol "Missing" not found: symbol not found');
  });

  test("write tool creates a temp file and returns syntax_valid", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "written.ts");
    const content = 'export function greetWorld(): string {\n  return "hello world";\n}\n';

    const resultStr = await tools.aft_edit.execute(
      {
        mode: "write",
        file: filePath,
        content,
        create_dirs: false,
      },
      sdkCtx,
    );
    const result = JSON.parse(resultStr);

    expect(result.success).toBe(true);
    expect(result.syntax_valid).toBe(true);
    expect(result.file).toBe(filePath);

    // Verify the file was actually written
    const fileContent = await readFile(filePath, "utf-8");
    expect(fileContent).toBe(content);
  });

  test("edit_symbol replaces a function and returns backup_id and syntax_valid", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "editable.ts");
    const original = 'export function hello(): string {\n  return "hi";\n}\n';

    // First write the file
    await tools.aft_edit.execute({ mode: "write", file: filePath, content: original }, sdkCtx);

    // Now replace the symbol
    const newContent = 'export function hello(): string {\n  return "world";\n}\n';
    const resultStr = await tools.aft_edit.execute(
      {
        mode: "symbol",
        file: filePath,
        symbol: "hello",
        operation: "replace",
        content: newContent,
      },
      sdkCtx,
    );
    const result = JSON.parse(resultStr);

    expect(result.success).toBe(true);
    expect(result.backup_id).toBeDefined();
    expect(typeof result.backup_id).toBe("string");
    expect(result.symbol).toBe("hello");
    expect(result.operation).toBe("replace");

    // Verify the file was actually changed
    const fileContent = await readFile(filePath, "utf-8");
    expect(fileContent).toContain("world");
    expect(fileContent).not.toContain('"hi"');
  });

  test("undo restores the file after edit_symbol", async () => {
    createBridge();
    const editTools = aftPrefixedTools(createPluginContext(pool));
    const undoTools = safetyTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "undoable.ts");
    const original =
      "export function greet(name: string): string {\n  return `Hello, ${name}!`;\n}\n";

    // Write original file
    await editTools.aft_edit.execute({ mode: "write", file: filePath, content: original }, sdkCtx);

    // Edit the symbol
    const replacement =
      "export function greet(name: string): string {\n  return `Goodbye, ${name}!`;\n}\n";
    const editResult = JSON.parse(
      await editTools.aft_edit.execute(
        {
          mode: "symbol",
          file: filePath,
          symbol: "greet",
          operation: "replace",
          content: replacement,
        },
        sdkCtx,
      ),
    );
    expect(editResult.success).toBe(true);

    // Verify file was changed
    let content = await readFile(filePath, "utf-8");
    expect(content).toContain("Goodbye");

    // Undo the edit
    const undoResult = JSON.parse(
      await undoTools.aft_safety.execute({ op: "undo", filePath }, sdkCtx),
    );
    expect(undoResult.success).toBe(true);
    expect(undoResult.backup_id).toBeDefined();

    // Verify file was restored
    content = await readFile(filePath, "utf-8");
    expect(content).toContain("Hello");
    expect(content).not.toContain("Goodbye");
  });

  test("transaction success applies multiple file writes", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const file1 = resolve(tmpDir, "a.ts");
    const file2 = resolve(tmpDir, "b.ts");

    const resultStr = await tools.aft_edit.execute(
      {
        mode: "transaction",
        operations: [
          { file: file1, command: "write", content: "export const a = 1;\n" },
          { file: file2, command: "write", content: "export const b = 2;\n" },
        ],
      },
      sdkCtx,
    );
    const result = JSON.parse(resultStr);

    expect(result.success).toBe(true);
    expect(result.files_modified).toBe(2);
    expect(Array.isArray(result.results)).toBe(true);
    expect(result.results.length).toBe(2);

    // Verify both files were created
    const content1 = await readFile(file1, "utf-8");
    const content2 = await readFile(file2, "utf-8");
    expect(content1).toContain("a = 1");
    expect(content2).toContain("b = 2");
  });

  test("transaction rollback on syntax error", async () => {
    createBridge();
    const editTools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    // Create a valid file that should be restored on rollback
    const existingFile = resolve(tmpDir, "existing.ts");
    const originalContent = "export const x = 1;\n";
    await editTools.aft_edit.execute(
      { mode: "write", file: existingFile, content: originalContent },
      sdkCtx,
    );

    // Transaction: write valid content to existing file, then write broken syntax to new file
    const brokenFile = resolve(tmpDir, "broken.ts");
    // Transaction should throw due to syntax error instead of reporting a
    // successful tool call with `{ success: false }` payload.
    await expect(
      editTools.aft_edit.execute(
        {
          mode: "transaction",
          operations: [
            { file: existingFile, command: "write", content: "export const x = 999;\n" },
            { file: brokenFile, command: "write", content: "export const {{{broken = ;\n" },
          ],
        },
        sdkCtx,
      ),
    ).rejects.toThrow("syntax error");

    // Existing file should be restored to original content
    const restoredContent = await readFile(existingFile, "utf-8");
    expect(restoredContent).toBe(originalContent);
  });

  // ---------------------------------------------------------------------
  // v0.17.2 footgun guards: edit must not silently overwrite a file when
  // the caller passes nonsense params. The previous behavior was that
  // `{ filePath, startLine, endLine, content }` (where startLine/endLine
  // are not valid top-level params) would silently degrade to "content-only
  // write" and overwrite the entire file. These tests lock in the new
  // explicit-failure behavior.
  // ---------------------------------------------------------------------
  test("edit rejects top-level startLine/endLine with a helpful pointer to edits[]", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "guarded.ts");
    const original = "export const x = 1;\n";
    await writeFile(filePath, original, "utf-8");

    let err: Error | undefined;
    try {
      await tools.aft_edit.execute(
        // No `mode` field, so this hits the modern (non-back-compat) path.
        // startLine/endLine are not valid top-level params on edit.
        { filePath, startLine: 1, endLine: 1, content: "export const x = 2;\n" },
        sdkCtx,
      );
    } catch (e) {
      err = e as Error;
    }
    expect(err).toBeDefined();
    expect(err!.message).toContain("startLine");
    expect(err!.message).toContain("edits");

    // File must be untouched — no silent overwrite.
    const after = await readFile(filePath, "utf-8");
    expect(after).toBe(original);
  });

  test("edit rejects content-only calls without an explicit edit mode", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "no-fallback.ts");
    const original = "export const y = 1;\n";
    await writeFile(filePath, original, "utf-8");

    let err: Error | undefined;
    try {
      await tools.aft_edit.execute(
        // `content` alone (no oldString, no symbol, no edits, no operations,
        // no legacy `mode: "write"`). Previously this silently overwrote the
        // file. Now it must fail with a pointer to the write tool.
        { filePath, content: "export const y = 2;\n" },
        sdkCtx,
      );
    } catch (e) {
      err = e as Error;
    }
    expect(err).toBeDefined();
    expect(err!.message).toContain("no edit mode resolved");
    expect(err!.message).toContain("aft_write");

    // File must be untouched.
    const after = await readFile(filePath, "utf-8");
    expect(after).toBe(original);
  });
});

describe("move_symbol round-trip", () => {
  let pool: BridgePool;
  let tmpDir: string;

  const TEST_TIMEOUT_MS = 15_000;

  afterEach(async () => {
    pool?.shutdown();
    if (tmpDir) await rm(tmpDir, { recursive: true, force: true });
  });

  test(
    "aft_move_symbol moves a function and rewires consumer import",
    async () => {
      pool = new BridgePool(
        BINARY_PATH,
        {
          timeoutMs: TEST_TIMEOUT_MS,
        },
        { harness: "opencode" },
      );

      // Create temp project with source, consumer, and destination
      tmpDir = await mkdtemp(resolve(tmpdir(), "aft-move-"));
      sdkCtx = createMockSdkContext(tmpDir);

      const sourceFile = resolve(tmpDir, "service.ts");
      const consumerFile = resolve(tmpDir, "consumer.ts");
      const destFile = resolve(tmpDir, "utils.ts");

      await writeFile(
        sourceFile,
        [
          "export function formatDate(date: Date): string {",
          "  return date.toISOString();",
          "}",
          "",
          "export function otherFn(): void {}",
          "",
        ].join("\n"),
      );

      await writeFile(
        consumerFile,
        [
          "import { formatDate } from './service';",
          "",
          "export function render(d: Date): string {",
          "  return formatDate(d);",
          "}",
          "",
        ].join("\n"),
      );

      // Move formatDate from service.ts to utils.ts
      const refTools = refactoringTools(createPluginContext(pool));
      const moveResult = JSON.parse(
        await refTools.aft_refactor.execute(
          {
            op: "move",
            filePath: sourceFile,
            symbol: "formatDate",
            destination: destFile,
          },
          sdkCtx,
        ),
      );

      expect(moveResult.success).toBe(true);
      expect(moveResult.files_modified).toBeGreaterThanOrEqual(2);

      // Verify response structure includes expected diagnostic fields
      expect(moveResult.consumers_updated).toBeDefined();
      expect(moveResult.checkpoint_name).toBeDefined();

      // Verify symbol was actually moved on disk
      const sourceContent = await readFile(sourceFile, "utf-8");
      expect(sourceContent).not.toContain("formatDate");
      expect(sourceContent).toContain("otherFn");

      const destContent = await readFile(destFile, "utf-8");
      expect(destContent).toContain("formatDate");

      // Verify consumer import was rewired
      const consumerContent = await readFile(consumerFile, "utf-8");
      expect(consumerContent).toContain("./utils");
      expect(consumerContent).not.toContain("./service");
    },
    TEST_TIMEOUT_MS,
  );
});

describe("extract_function round-trip", () => {
  let pool: BridgePool;
  let tmpDir: string;

  const TEST_TIMEOUT_MS = 15_000;

  afterEach(async () => {
    pool?.shutdown();
    if (tmpDir) await rm(tmpDir, { recursive: true, force: true });
  });

  test(
    "aft_extract_function extracts code range into a new function with parameters",
    async () => {
      pool = new BridgePool(
        BINARY_PATH,
        {
          timeoutMs: TEST_TIMEOUT_MS,
        },
        { harness: "opencode" },
      );

      tmpDir = await mkdtemp(resolve(tmpdir(), "aft-extract-"));
      sdkCtx = createMockSdkContext(tmpDir);

      const filePath = resolve(tmpDir, "source.ts");
      await writeFile(
        filePath,
        [
          "function processData(items: string[], prefix: string): string {",
          "  const filtered = items.filter(item => item.length > 0);",
          "  const mapped = filtered.map(item => prefix + item);",
          '  const result = mapped.join(", ");',
          "  return result;",
          "}",
          "",
        ].join("\n"),
      );

      // Extract lines 1-3 (the filtering and mapping logic)
      const refTools = refactoringTools(createPluginContext(pool));
      const result = JSON.parse(
        await refTools.aft_refactor.execute(
          {
            op: "extract",
            filePath,
            name: "filterAndMap",
            startLine: 1,
            endLine: 4,
          },
          sdkCtx,
        ),
      );

      expect(result.success).toBe(true);
      expect(Array.isArray(result.parameters)).toBe(true);
      expect(result.parameters.length).toBeGreaterThan(0);
      expect(result.return_type).toBeDefined();
      const content = await readFile(filePath, "utf-8");
      expect(content).toContain("function filterAndMap");
    },
    TEST_TIMEOUT_MS,
  );
});

describe("inline_symbol round-trip", () => {
  let pool: BridgePool;
  let tmpDir: string;

  const TEST_TIMEOUT_MS = 15_000;

  afterEach(async () => {
    pool?.shutdown();
    if (tmpDir) await rm(tmpDir, { recursive: true, force: true });
  });

  test(
    "aft_inline_symbol inlines a function call and returns substitution info",
    async () => {
      pool = new BridgePool(
        BINARY_PATH,
        {
          timeoutMs: TEST_TIMEOUT_MS,
        },
        { harness: "opencode" },
      );

      tmpDir = await mkdtemp(resolve(tmpdir(), "aft-inline-"));
      sdkCtx = createMockSdkContext(tmpDir);

      const filePath = resolve(tmpDir, "source.ts");
      await writeFile(
        filePath,
        [
          "function helper(a: number, b: number): number {",
          "  return a + b;",
          "}",
          "",
          "function main() {",
          "  const result = helper(10, 20);",
          "  console.log(result);",
          "}",
          "",
        ].join("\n"),
      );

      // Inline helper at line 6 (const result = helper(10, 20)) — 1-based
      const refTools = refactoringTools(createPluginContext(pool));
      const result = JSON.parse(
        await refTools.aft_refactor.execute(
          {
            op: "inline",
            filePath,
            symbol: "helper",
            callSiteLine: 6,
          },
          sdkCtx,
        ),
      );

      expect(result.success).toBe(true);
      expect(result.symbol).toBe("helper");
      expect(result.call_context).toBe("assignment");
      expect(result.substitutions).toBeGreaterThan(0);

      // Verify file was modified
      const content = await readFile(filePath, "utf-8");
      expect(content).not.toContain("helper(10, 20)");
    },
    TEST_TIMEOUT_MS,
  );
});
