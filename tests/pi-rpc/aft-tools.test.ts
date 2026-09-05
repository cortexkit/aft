import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { withHermeticGitEnv } from "../helpers/git-env.js";
import {
  cleanupPiIsolatedEnv,
  createPiIsolatedEnv,
  type PiIsolatedEnv,
  type RpcClient,
  resolvePiPluginDir,
  spawnPiRpc,
  startAimock,
} from "./helpers";

async function enableAllToolSurface(env: PiIsolatedEnv): Promise<void> {
  await mkdir(join(env.workdir, ".pi"), { recursive: true });
  await writeFile(
    join(env.workdir, ".pi", "aft.jsonc"),
    JSON.stringify({ tool_surface: "all", search_index: true, semantic_search: false }),
    "utf8",
  );
}

async function enableAllToolSurfaceWithSearch(env: PiIsolatedEnv): Promise<void> {
  await mkdir(join(env.workdir, ".pi"), { recursive: true });
  await writeFile(
    join(env.workdir, ".pi", "aft.jsonc"),
    JSON.stringify({ tool_surface: "all", search_index: true, semantic_search: true }),
    "utf8",
  );
}

function resultText(event: Record<string, unknown>): string {
  const result = event.result;
  if (result && typeof result === "object" && !Array.isArray(result)) {
    const content = (result as { content?: unknown }).content;
    if (Array.isArray(content)) {
      return content
        .map((part) => {
          if (!part || typeof part !== "object" || Array.isArray(part)) return "";
          const text = (part as { text?: unknown }).text;
          return typeof text === "string" ? text : "";
        })
        .filter((text) => text.length > 0)
        .join("\n");
    }
  }
  return JSON.stringify(result ?? "");
}

async function setupMergeConflict(env: PiIsolatedEnv): Promise<void> {
  const runGit = (args: string[]) =>
    execFileSync("git", args, { cwd: env.workdir, env: withHermeticGitEnv(), stdio: "ignore" });
  // Pin the initial branch: `init.defaultBranch` differs between hosts (CI runners
  // give master, developer machines often main) and the checkout below names it.
  runGit(["init", "-b", "master"]);
  runGit(["config", "user.email", "pi@example.test"]);
  runGit(["config", "user.name", "Pi Test"]);
  await writeFile(join(env.workdir, "conflict.txt"), "base\n", "utf8");
  runGit(["add", "conflict.txt"]);
  runGit(["commit", "-m", "base"]);
  runGit(["checkout", "-b", "feature"]);
  await writeFile(join(env.workdir, "conflict.txt"), "feature\n", "utf8");
  runGit(["commit", "-am", "feature"]);
  runGit(["checkout", "master"]);
  await writeFile(join(env.workdir, "conflict.txt"), "main\n", "utf8");
  runGit(["commit", "-am", "main"]);
  try {
    runGit(["merge", "feature"]);
  } catch {
    // The merge is expected to stop with conflict markers for aft_conflicts.
  }
}

async function waitForToolEnds(
  client: RpcClient,
  count: number,
): Promise<Record<string, unknown>[]> {
  const seen = new Set<string>();
  const events: Record<string, unknown>[] = [];
  for (let index = 0; index < count; index += 1) {
    const event = await client.waitForEvent((candidate) => {
      if (candidate.type !== "tool_execution_end") return false;
      const id = typeof candidate.toolCallId === "string" ? candidate.toolCallId : undefined;
      return id === undefined || !seen.has(id);
    }, 60_000);
    if (typeof event.toolCallId === "string") seen.add(event.toolCallId);
    events.push(event);
  }
  return events;
}

function eventsForTool(
  events: Record<string, unknown>[],
  toolName: string,
): Record<string, unknown>[] {
  return events.filter((event) => event.toolName === toolName);
}

function requestLastUserTextIncludes(request: unknown, text: string): boolean {
  if (!request || typeof request !== "object" || Array.isArray(request)) return false;
  const messages = (request as { messages?: unknown }).messages;
  if (!Array.isArray(messages)) return false;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (!message || typeof message !== "object" || Array.isArray(message)) continue;
    if ((message as { role?: unknown }).role !== "user") continue;
    return JSON.stringify(message).includes(text);
  }
  return false;
}

async function runPromptedTool(
  client: RpcClient,
  aimock: Awaited<ReturnType<typeof startAimock>>,
  seenToolCallIds: Set<string>,
  toolCall: { name: string; arguments: Record<string, unknown> },
  message: string,
): Promise<Record<string, unknown>> {
  aimock.registerToolCallFixture({
    predicate: (request) => requestLastUserTextIncludes(request, message),
    toolCalls: [toolCall],
    followupText: "Done.",
  });
  expect(
    (await client.sendCommand({ type: "prompt", message, streamingBehavior: "followUp" })).success,
  ).toBe(true);
  const event = await client.waitForEvent((candidate) => {
    if (candidate.type !== "tool_execution_end" || candidate.toolName !== toolCall.name) {
      return false;
    }
    const id = typeof candidate.toolCallId === "string" ? candidate.toolCallId : undefined;
    return id === undefined || !seenToolCallIds.has(id);
  }, 60_000);
  if (typeof event.toolCallId === "string") seenToolCallIds.add(event.toolCallId);
  return event;
}

async function withPiTool(
  toolCall: { name: string; arguments: Record<string, unknown> },
  opts: {
    message: string;
    setup?: (env: PiIsolatedEnv) => Promise<void>;
  },
): Promise<Record<string, unknown>> {
  const env = createPiIsolatedEnv();
  const aimock = await startAimock();
  let client: RpcClient | undefined;
  try {
    await opts.setup?.(env);
    aimock.registerToolCallFixture({
      predicate: () => true,
      toolCalls: [toolCall],
      followupText: "Done.",
    });
    const spawned = spawnPiRpc({
      mockProviderURL: aimock.url,
      aftPluginDir: resolvePiPluginDir(),
      configDir: env.configDir,
      workdir: env.workdir,
    });
    client = spawned.client;
    expect(spawned.child.pid).toBeGreaterThan(0);
    expect((await client.sendCommand({ type: "prompt", message: opts.message })).success).toBe(
      true,
    );
    return await client.waitForEvent(
      (event) => event.type === "tool_execution_end" && event.toolName === toolCall.name,
      30_000,
    );
  } finally {
    await client?.close();
    await aimock.close();
    await cleanupPiIsolatedEnv(env);
  }
}

describe("AFT Pi tools (real Pi RPC)", () => {
  test("aft_callgraph trace_to_symbol returns a reachable path", async () => {
    const toolEnd = await withPiTool(
      {
        name: "aft_callgraph",
        arguments: {
          op: "trace_to_symbol",
          filePath: "trace.ts",
          symbol: "source",
          toSymbol: "target",
          toFile: "trace.ts",
        },
      },
      {
        message: "Trace source to target in trace.ts.",
        setup: async (env) => {
          await enableAllToolSurface(env);
          await writeFile(
            join(env.workdir, "trace.ts"),
            [
              "export function source(): string { return middle(); }",
              "function middle(): string { return target(); }",
              "export function target(): string { return 'target'; }",
              "",
            ].join("\n"),
            "utf8",
          );
        },
      },
    );

    expect(toolEnd.isError).toBe(false);
    // aft_callgraph returns flat text to the agent (structured data is carried
    // in `details` for richer renderers). Assert on the flat output.
    const text = resultText(toolEnd);
    const hopMatch = text.match(/(\d+) hops?/);
    expect(hopMatch).not.toBeNull();
    expect(Number(hopMatch?.[1])).toBe(3);
    // Hop order: source → middle → target, in that sequence.
    const sourceIdx = text.indexOf("source");
    const middleIdx = text.indexOf("middle");
    const targetIdx = text.indexOf("target");
    expect(sourceIdx).toBeGreaterThanOrEqual(0);
    expect(middleIdx).toBeGreaterThan(sourceIdx);
    expect(targetIdx).toBeGreaterThan(middleIdx);
  }, 120_000);

  test("aft_outline files mode returns directory file metadata", async () => {
    const toolEnd = await withPiTool(
      {
        name: "aft_outline",
        arguments: {
          target: "src",
          files: true,
        },
      },
      {
        message: "Outline files under src.",
        setup: async (env) => {
          await mkdir(join(env.workdir, "src"), { recursive: true });
          await writeFile(
            join(env.workdir, "src", "one.ts"),
            "export function one() { return 1; }\n",
            "utf8",
          );
          await writeFile(join(env.workdir, "src", "two.py"), "def two():\n    return 2\n", "utf8");
        },
      },
    );

    expect(toolEnd.isError).toBe(false);
    const text = resultText(toolEnd);
    expect(text).toMatch(/one\.ts\s+typescript\s+1 syms\s+\d+ bytes/);
    expect(text).toMatch(/two\.py\s+python\s+1 syms\s+\d+ bytes/);
  }, 120_000);

  test("aft_outline file mode returns server-rendered symbol text", async () => {
    const toolEnd = await withPiTool(
      {
        name: "aft_outline",
        arguments: { target: "src/one.ts" },
      },
      {
        message: "Outline src/one.ts.",
        setup: async (env) => {
          await enableAllToolSurface(env);
          await mkdir(join(env.workdir, "src"), { recursive: true });
          await writeFile(
            join(env.workdir, "src", "one.ts"),
            "export function one() { return 1; }\n",
            "utf8",
          );
        },
      },
    );

    expect(toolEnd.isError).toBe(false);
    const text = resultText(toolEnd);
    expect(text).toContain("one.ts");
    expect(text).toContain("function one");
  }, 120_000);

  test("aft_search returns Rust text for literal fallback search", async () => {
    const toolEnd = await withPiTool(
      {
        name: "aft_search",
        arguments: { query: "needle", topK: 5, includeTests: true },
      },
      {
        message: "Search for needle.",
        setup: async (env) => {
          await enableAllToolSurfaceWithSearch(env);
          await writeFile(
            join(env.workdir, "needle.ts"),
            "export const value = 'needle';\n",
            "utf8",
          );
        },
      },
    );

    expect(toolEnd.isError).toBe(false);
    const text = resultText(toolEnd);
    expect(text).toContain("needle.ts");
    // Exact phrasing depends on semantic backend availability in the runner
    // ("Found 1 match" with ONNX present, "Found 1 lexical fallback result(s)"
    // without); the contract under test is server-rendered text passthrough.
    expect(text).toContain("Found 1");
  }, 120_000);

  test("aft_inspect returns server-rendered health text", async () => {
    const toolEnd = await withPiTool(
      {
        name: "aft_inspect",
        arguments: { sections: "todos", topK: 5 },
      },
      {
        message: "Inspect the project.",
        setup: async (env) => {
          await enableAllToolSurface(env);
          await writeFile(
            join(env.workdir, "todo.ts"),
            "// TODO: check me\nexport const value = 1;\n",
            "utf8",
          );
        },
      },
    );

    expect(toolEnd.isError).toBe(false);
    const text = resultText(toolEnd);
    expect(text).toContain("TODO");
    expect(text).not.toContain('"summary"');
  }, 120_000);

  test("aft_safety returns server-rendered text and preserves safety mutations", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    let client: RpcClient | undefined;
    try {
      await enableAllToolSurface(env);
      await writeFile(join(env.workdir, "checkpoint.txt"), "checkpoint contents\n", "utf8");
      await writeFile(join(env.workdir, "history-undo.txt"), "undo me\n", "utf8");
      await writeFile(join(env.workdir, "op-one.txt"), "one\n", "utf8");
      await writeFile(join(env.workdir, "op-two.txt"), "two\n", "utf8");
      await writeFile(join(env.workdir, "restore.txt"), "restore me\n", "utf8");

      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;
      const seenToolCallIds = new Set<string>();

      const checkpoint = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        {
          name: "aft_safety",
          arguments: { op: "checkpoint", name: "single-file", filePath: "checkpoint.txt" },
        },
        "safety checkpoint single-file",
      );
      expect(checkpoint.isError).toBe(false);
      const checkpointText = resultText(checkpoint);
      expect(checkpointText).toContain("checkpoint created single-file");
      expect(checkpointText).not.toContain('"success"');

      const list = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_safety", arguments: { op: "list" } },
        "safety list checkpoints",
      );
      expect(list.isError).toBe(false);
      expect(resultText(list)).toContain("1 checkpoint(s)");

      const deleteForHistory = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_delete", arguments: { files: ["history-undo.txt"] } },
        "delete history undo file",
      );
      expect(deleteForHistory.isError).toBe(false);

      const history = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_safety", arguments: { op: "history", filePath: "history-undo.txt" } },
        "safety history file",
      );
      expect(history.isError).toBe(false);
      const historyText = resultText(history);
      expect(historyText).toContain("history-undo.txt");
      expect(historyText).toContain("delete_file: pre-delete backup");
      expect(historyText).not.toContain('"entries"');

      const undoFile = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_safety", arguments: { op: "undo", filePath: "history-undo.txt" } },
        "safety undo file",
      );
      expect(undoFile.isError).toBe(false);
      expect(resultText(undoFile)).toContain("restored history-undo.txt");
      expect(await readFile(join(env.workdir, "history-undo.txt"), "utf8")).toBe("undo me\n");

      const deleteForOperation = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_delete", arguments: { files: ["op-one.txt", "op-two.txt"] } },
        "delete operation files",
      );
      expect(deleteForOperation.isError).toBe(false);

      const undoOperation = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_safety", arguments: { op: "undo" } },
        "safety undo operation",
      );
      expect(undoOperation.isError).toBe(false);
      const undoOperationText = resultText(undoOperation);
      expect(undoOperationText).toContain("restored operation");
      expect(undoOperationText).toContain("files 2");
      expect(await readFile(join(env.workdir, "op-one.txt"), "utf8")).toBe("one\n");
      expect(await readFile(join(env.workdir, "op-two.txt"), "utf8")).toBe("two\n");

      const checkpointForRestore = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        {
          name: "aft_safety",
          arguments: { op: "checkpoint", name: "restore-cp", filePath: "restore.txt" },
        },
        "safety checkpoint restore cp",
      );
      expect(checkpointForRestore.isError).toBe(false);
      const deleteForRestore = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_delete", arguments: { files: ["restore.txt"] } },
        "delete restore file",
      );
      expect(deleteForRestore.isError).toBe(false);
      expect(existsSync(join(env.workdir, "restore.txt"))).toBe(false);

      const restore = await runPromptedTool(
        client,
        aimock,
        seenToolCallIds,
        { name: "aft_safety", arguments: { op: "restore", name: "restore-cp" } },
        "safety restore cp",
      );
      expect(restore.isError).toBe(false);
      const restoreText = resultText(restore);
      expect(restoreText).toContain("checkpoint restored restore-cp");
      expect(restoreText).toContain("files 1");
      expect(await readFile(join(env.workdir, "restore.txt"), "utf8")).toBe("restore me\n");
    } finally {
      await client?.close();
      await aimock.close();
      await cleanupPiIsolatedEnv(env);
    }
  }, 120_000);

  test("remaining Pi tools use tool_call text and keep disk mutations", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    let client: RpcClient | undefined;
    try {
      await enableAllToolSurface(env);
      await setupMergeConflict(env);
      await writeFile(
        join(env.workdir, "ast-search.ts"),
        "export function searched() {\n  console.log('search');\n}\n",
        "utf8",
      );
      await writeFile(
        join(env.workdir, "ast-dry.ts"),
        "export function dryRun() {\n  console.log('dry');\n}\n",
        "utf8",
      );
      await writeFile(
        join(env.workdir, "ast-apply.ts"),
        "export function applyRun() {\n  console.log('apply');\n}\n",
        "utf8",
      );
      await writeFile(
        join(env.workdir, "import-add.ts"),
        "export const value = join('a', 'b');\n",
        "utf8",
      );
      await writeFile(
        join(env.workdir, "import-organize.ts"),
        "import { b } from './b';\nimport { a } from './a';\nexport const value = a + b;\n",
        "utf8",
      );
      await writeFile(join(env.workdir, "delete-me.txt"), "delete\n", "utf8");
      await writeFile(join(env.workdir, "move-me.txt"), "move\n", "utf8");

      const toolCalls = [
        { name: "aft_conflicts", arguments: { path: "." } },
        {
          name: "ast_grep_search",
          arguments: {
            pattern: "console.log($MSG)",
            lang: "typescript",
            paths: ["ast-search.ts"],
            contextLines: 1,
          },
        },
        {
          name: "ast_grep_replace",
          arguments: {
            pattern: "console.log($MSG)",
            rewrite: "logger.info($MSG)",
            lang: "typescript",
            paths: ["ast-dry.ts"],
            dryRun: true,
          },
        },
        {
          name: "ast_grep_replace",
          arguments: {
            pattern: "console.log($MSG)",
            rewrite: "logger.info($MSG)",
            lang: "typescript",
            paths: ["ast-apply.ts"],
            dryRun: false,
          },
        },
        {
          name: "aft_import",
          arguments: {
            op: "add",
            filePath: "import-add.ts",
            module: "node:path",
            names: ["join"],
            validate: "syntax",
          },
        },
        {
          name: "aft_import",
          arguments: { op: "organize", filePath: "import-organize.ts", validate: "syntax" },
        },
        { name: "aft_delete", arguments: { files: ["delete-me.txt"] } },
        {
          name: "aft_move",
          arguments: { filePath: "move-me.txt", destination: "moved/move-me.txt" },
        },
      ];
      aimock.registerToolCallFixture({ predicate: () => true, toolCalls, followupText: "Done." });
      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;
      expect(
        (await client.sendCommand({ type: "prompt", message: "Run remaining AFT tools." })).success,
      ).toBe(true);
      const events = await waitForToolEnds(client, toolCalls.length);

      for (const event of events) expect(event.isError).toBe(false);
      const conflictText = resultText(eventsForTool(events, "aft_conflicts")[0]);
      expect(conflictText).toContain("1 file, 1 conflict");
      expect(conflictText).toContain("conflict.txt");

      const astSearchText = resultText(eventsForTool(events, "ast_grep_search")[0]);
      expect(astSearchText).toContain("Found 1 match(es) in 1 file(s)");
      expect(astSearchText).toContain("ast-search.ts");
      expect(astSearchText).not.toContain('"matches"');

      const astReplaceTexts = eventsForTool(events, "ast_grep_replace").map(resultText);
      expect(
        astReplaceTexts.some((text) => text.includes("[DRY RUN] Would replace 1 match(es)")),
      ).toBe(true);
      expect(astReplaceTexts.some((text) => text.includes("Replaced 1 match(es)"))).toBe(true);
      expect(await readFile(join(env.workdir, "ast-dry.ts"), "utf8")).toContain(
        "console.log('dry')",
      );
      expect(await readFile(join(env.workdir, "ast-apply.ts"), "utf8")).toContain(
        "logger.info('apply')",
      );

      const importTexts = eventsForTool(events, "aft_import").map(resultText);
      expect(importTexts.some((text) => text.includes("added node:path"))).toBe(true);
      expect(importTexts.some((text) => text.includes("organized"))).toBe(true);
      expect(await readFile(join(env.workdir, "import-add.ts"), "utf8")).toContain("node:path");
      expect(await readFile(join(env.workdir, "import-organize.ts"), "utf8")).toContain(
        "import { a } from './a';",
      );

      const deleteText = resultText(eventsForTool(events, "aft_delete")[0]);
      expect(deleteText).toContain("Deleted");
      expect(existsSync(join(env.workdir, "delete-me.txt"))).toBe(false);

      const moveText = resultText(eventsForTool(events, "aft_move")[0]);
      expect(moveText).toContain("Moved move-me.txt → moved/move-me.txt");
      expect(existsSync(join(env.workdir, "move-me.txt"))).toBe(false);
      expect(existsSync(join(env.workdir, "moved", "move-me.txt"))).toBe(true);
    } finally {
      await client?.close();
      await aimock.close();
      await cleanupPiIsolatedEnv(env);
    }
  }, 120_000);

  test("aft_callgraph callers returns text and soft symbol_not_found stays non-error", async () => {
    const env = createPiIsolatedEnv();
    const aimock = await startAimock();
    let client: RpcClient | undefined;
    try {
      await enableAllToolSurface(env);
      await writeFile(
        join(env.workdir, "callers.ts"),
        [
          "export function caller() { return target(); }",
          "export function target() { return 'ok'; }",
          "",
        ].join("\n"),
        "utf8",
      );
      aimock.registerToolCallFixture({
        predicate: () => true,
        toolCalls: [
          {
            name: "aft_callgraph",
            arguments: { op: "callers", filePath: "callers.ts", symbol: "target" },
          },
          {
            name: "aft_callgraph",
            arguments: { op: "callers", filePath: "callers.ts", symbol: "missingSymbol" },
          },
        ],
        followupText: "Done.",
      });
      const spawned = spawnPiRpc({
        mockProviderURL: aimock.url,
        aftPluginDir: resolvePiPluginDir(),
        configDir: env.configDir,
        workdir: env.workdir,
      });
      client = spawned.client;
      expect(
        (await client.sendCommand({ type: "prompt", message: "Run two callgraph checks." }))
          .success,
      ).toBe(true);
      const callerResult = await client.waitForEvent(
        (event) =>
          event.type === "tool_execution_end" &&
          event.toolName === "aft_callgraph" &&
          event.isError === false &&
          resultText(event).includes("caller"),
        30_000,
      );
      const missingSymbolResult = await client.waitForEvent(
        (event) =>
          event.type === "tool_execution_end" &&
          event.toolName === "aft_callgraph" &&
          event.isError === false &&
          resultText(event).includes("symbol_not_found"),
        30_000,
      );

      expect(callerResult.isError).toBe(false);
      expect(resultText(callerResult)).toContain("caller");
      expect(missingSymbolResult.isError).toBe(false);
      expect(resultText(missingSymbolResult)).toContain("symbol_not_found");
    } finally {
      await client?.close();
      await aimock.close();
      await cleanupPiIsolatedEnv(env);
    }
  }, 120_000);
});
