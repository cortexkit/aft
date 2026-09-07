/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { aftPrefixedTools } from "../tools/hoisted.js";
import { formatZoomBatchResult, readingTools } from "../tools/reading.js";
import { safetyTools } from "../tools/safety.js";
import type { PluginContext } from "../types.js";
import { noopAsk, toolResultText } from "./test-helpers";

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

    // Signature lines carry the declaration text itself; the minimal E
    // marker appears only when the signature lacks a visibility keyword
    // (TypeScript exports live on the wrapping statement).
    expect(text).toContain("E function greet"); // exported function
    expect(text).toContain("E class UserService"); // exported class
    expect(text).toContain("function internalHelper"); // internal: bare signature
    expect(text).not.toContain("E function internalHelper");
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

  test("OpenCode-prefixed aft_edit rejects the retired mode/file form", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "written.ts");
    await expect(
      tools.aft_edit.execute(
        { mode: "write", file: filePath, content: "export const value = 1;\n" },
        sdkCtx,
      ),
    ).rejects.toThrow("retired");
    expect(await readFile(filePath, "utf8").catch(() => "missing")).toBe("missing");
  });

  test("edit_symbol replaces a function and returns backup_id and syntax_valid", async () => {
    createBridge();
    const tools = aftPrefixedTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(tmpdir(), "aft-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "editable.ts");
    const original = 'export function hello(): string {\n  return "hi";\n}\n';

    // First write the file
    await tools.aft_write.execute({ path: filePath, content: original }, sdkCtx);

    // Now replace the symbol
    const newContent = 'export function hello(): string {\n  return "world";\n}\n';
    const resultStr = toolResultText(
      await tools.aft_edit.execute(
        {
          path: filePath,
          symbol: "hello",
          content: newContent,
        },
        sdkCtx,
      ),
    );
    // Agent-facing output is the compact summary, not raw JSON.
    expect(resultStr).toMatch(/^Edited \(\+\d+\/-\d+\)/);

    // Behavior is verified from disk: the symbol body was actually replaced.
    const fileContent = await readFile(filePath, "utf-8");
    expect(fileContent).toContain("world");
    expect(fileContent).not.toContain('"hi"');
  });

  test("undo restores the file after edit_symbol", async () => {
    createBridge();
    const editTools = aftPrefixedTools(createPluginContext(pool));
    const undoTools = safetyTools(createPluginContext(pool));
    tmpDir = await mkdtemp(resolve(PROJECT_CWD, "target", "aft-undo-test-"));
    sdkCtx = createMockSdkContext(tmpDir);

    const filePath = resolve(tmpDir, "undoable.ts");
    const original =
      "export function greet(name: string): string {\n  return `Hello, ${name}!`;\n}\n";

    // Write original file
    await editTools.aft_write.execute({ path: filePath, content: original }, sdkCtx);

    // Edit the symbol
    const replacement =
      "export function greet(name: string): string {\n  return `Goodbye, ${name}!`;\n}\n";
    const editResult = toolResultText(
      await editTools.aft_edit.execute(
        {
          path: filePath,
          symbol: "greet",
          content: replacement,
        },
        sdkCtx,
      ),
    );
    expect(editResult).toMatch(/^Edited \(\+\d+\/-\d+\)/);

    // Verify file was changed
    let content = await readFile(filePath, "utf-8");
    expect(content).toContain("Goodbye");

    // Undo the edit
    const undoResult = toolResultText(
      await undoTools.aft_safety.execute({ op: "undo", filePath }, sdkCtx),
    );
    expect(undoResult).toContain("restored");
    expect(undoResult.trim().startsWith("{")).toBe(false);

    // Verify file was restored
    content = await readFile(filePath, "utf-8");
    expect(content).toContain("Hello");
    expect(content).not.toContain("Goodbye");
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
        // file. Now it must fail instead of silently choosing a write mode.
        { filePath, content: "export const y = 2;\n" },
        sdkCtx,
      );
    } catch (e) {
      err = e as Error;
    }
    expect(err).toBeDefined();
    expect(err!.message).toContain("symbol");

    // File must be untouched.
    const after = await readFile(filePath, "utf-8");
    expect(after).toBe(original);
  });
});
