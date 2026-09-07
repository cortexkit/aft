/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { withHermeticGitEnv } from "../../../../../tests/helpers/git-env.js";

import { semanticTools } from "../../tools/semantic.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  configureParamsFromLegacyOverrides,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

// The semantic lane needs ONNX Runtime. The real plugin resolves it via
// ensureOnnxRuntime and threads `_ort_dylib_dir` into configure; this harness
// must do the same or the bridge's bare dlopen fails and the lane reports
// "unavailable" on every machine. Probe the standard cached locations only
// (never download): when a local ORT exists the ready-path assertions below
// become a REAL end-to-end check of the semantic lane; when it doesn't, the
// test honestly asserts the degraded contract instead of a state the harness
// cannot reach.
function findLocalOrtDir(): string | undefined {
  const libName =
    process.platform === "darwin"
      ? "libonnxruntime.dylib"
      : process.platform === "win32"
        ? "onnxruntime.dll"
        : "libonnxruntime.so";
  const dataHome =
    process.env.XDG_DATA_HOME ??
    (process.platform === "win32"
      ? (process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local"))
      : join(homedir(), ".local", "share"));
  const ortRoot = join(dataHome, "cortexkit", "aft", "onnxruntime");
  try {
    for (const version of readdirSync(ortRoot)) {
      for (const dir of [join(ortRoot, version), join(ortRoot, version, "lib")]) {
        if (existsSync(join(dir, libName))) return dir;
      }
    }
  } catch {
    // No cached ORT — semantic lane will be unavailable; degraded contract applies.
  }
  return undefined;
}

const localOrtDir = findLocalOrtDir();

// The bridge points FASTEMBED_CACHE_DIR at `<storage_dir>/semantic/models`, and
// this suite gives every run a throwaway storage dir - so without this, each
// run re-downloads the 90 MB MiniLM model from Hugging Face and the ready-path
// assertion silently depends on the network. Reuse the developer's warm
// fastembed cache when it already holds the model: the blob is immutable and
// content-addressed, so sharing it costs no isolation. A cold machine (CI)
// takes the download path exactly as before.
function findWarmFastembedCache(): string | undefined {
  const cacheDir = process.env.FASTEMBED_CACHE_DIR ?? join(homedir(), ".cache", "fastembed");
  const snapshots = join(cacheDir, "models--Qdrant--all-MiniLM-L6-v2-onnx", "snapshots");
  try {
    for (const revision of readdirSync(snapshots)) {
      const snapshot = join(snapshots, revision);
      if (
        existsSync(join(snapshot, "model.onnx")) &&
        existsSync(join(snapshot, "tokenizer.json"))
      ) {
        return cacheDir;
      }
    }
  } catch {
    // No warm cache - the bridge's per-storage download path applies.
  }
  return undefined;
}

const warmFastembedCache = findWarmFastembedCache();

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

function createPluginContext(pool: BridgePool, storageDir: string): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir,
  };
}

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: "semantic-search-e2e",
    messageID: "semantic-search-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

maybeDescribe("e2e semantic search tool", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];
  const pools: BridgePool[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(pools.splice(0, pools.length).map((pool) => pool.shutdown()));
    await cleanupHarnesses(harnesses);
  });

  async function createToolHarness(options?: { experimentalSemanticSearch?: boolean }) {
    const harness = await createHarness(preparedBinary, {
      fixtureNames: [],
      timeoutMs: 20_000,
      tempPrefix: "aft-plugin-semantic-search-",
    });
    harnesses.push(harness);

    await createFixtureProject(harness.tempDir);

    const pool = new BridgePool(
      harness.binaryPath,
      {
        timeoutMs: 20_000,
        ...(warmFastembedCache ? { childEnv: { FASTEMBED_CACHE_DIR: warmFastembedCache } } : {}),
      },
      configureParamsFromLegacyOverrides({
        semantic_search: options?.experimentalSemanticSearch ?? false,
        storage_dir: join(harness.tempDir, ".storage"),
        harness: "opencode",
        ...(options?.experimentalSemanticSearch && localOrtDir
          ? { _ort_dylib_dir: localOrtDir }
          : {}),
      }),
    );
    pools.push(pool);

    return {
      harness,
      pool,
      sdkCtx: createSdkContext(harness.tempDir),
      tools: semanticTools(createPluginContext(pool, join(harness.tempDir, ".storage"))),
    };
  }

  test("aft_search degrades to a lexical fallback when semantic is disabled", async () => {
    const { tools, sdkCtx } = await createToolHarness({ experimentalSemanticSearch: false });

    const output = await tools.aft_search.execute(
      { query: "request authentication handler" },
      sdkCtx,
    );

    // With semantic disabled, a natural-language query degrades to a lexical
    // (literal grep) fallback rather than stranding the agent with zero
    // results. The response stays honest — it still names that semantic is
    // unavailable — but returns usable lexical matches. (Matches the v0.32
    // degraded-fallback contract; see aft_search_contract_test.)
    expect(typeof output).toBe("string");
    expect(output).toContain("Semantic search is not enabled.");
  });

  test("aft_search handles a missing query parameter gracefully", async () => {
    const { tools, sdkCtx } = await createToolHarness({ experimentalSemanticSearch: false });

    await expect(tools.aft_search.execute({ topK: 3 } as never, sdkCtx)).rejects.toThrow(
      /missing field `query`|invalid params/i,
    );
  });

  test("aft_search with a valid query returns formatted text", async () => {
    const { tools, sdkCtx } = await createToolHarness({ experimentalSemanticSearch: true });

    const classify = (text: string) => ({
      isBuilding:
        text.includes("building") || text.includes("not ready") || text.includes("not_ready"),
      isUnavailable:
        text.includes("unavailable") ||
        text.includes("ONNX") ||
        text.includes("not found") ||
        text.includes("not enabled"),
      isDisabled: text.includes("disabled") || text.includes("not enabled"),
    });

    const runQuery = () =>
      tools.aft_search.execute(
        { query: "request authentication handler" },
        sdkCtx,
      ) as Promise<string>;

    let output = await runQuery();
    expect(typeof output).toBe("string");
    expect(output.length).toBeGreaterThan(0);

    if (localOrtDir) {
      // ORT is threaded, so "unavailable" would be a REAL regression here. The
      // cold index may still be building on the first call — poll briefly for
      // readiness, then assert the full ready-path contract.
      const deadline = Date.now() + 30_000;
      let state = classify(output);
      while (state.isBuilding && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 500));
        output = await runQuery();
        state = classify(output);
      }
      expect(state.isBuilding || state.isUnavailable || state.isDisabled).toBe(false);
      expect(output).toContain("Found ");
      // The ready path no longer carries an [index: ready] tag (absence == ready,
      // for both the semantic and lexical lanes) — it was per-call token tax.
      expect(output).not.toContain("[index: ready]");
      expect(output).toContain("src/");
      return;
    }

    // No local ORT cache: the semantic lane is unavailable by construction.
    // Assert the honest degraded contract instead of skipping silently.
    const state = classify(output);
    expect(state.isUnavailable || state.isBuilding || state.isDisabled).toBe(true);
    expect(output).toContain("lexical");
  }, 40_000);

  test("aft_search can borrow an already-indexed sibling project", async () => {
    const harness = await createHarness(preparedBinary, {
      fixtureNames: [],
      timeoutMs: 20_000,
      tempPrefix: "aft-plugin-external-search-",
    });
    harnesses.push(harness);

    const projectA = join(harness.tempDir, "project-a");
    const projectB = join(harness.tempDir, "project-b");
    await Promise.all([createFixtureProject(projectA), createFixtureProject(projectB)]);
    initGit(projectA);
    initGit(projectB);

    const storageDir = join(harness.tempDir, ".storage");
    const pool = new BridgePool(
      harness.binaryPath,
      { timeoutMs: 20_000 },
      configureParamsFromLegacyOverrides({
        search_index: true,
        semantic_search: false,
        storage_dir: storageDir,
        harness: "opencode",
      }),
    );
    pools.push(pool);

    await pool.getBridge(projectA).send("semantic_search", {
      query: "handle_request",
      hint: "literal",
      top_k: 5,
    });

    const tools = semanticTools(createPluginContext(pool, storageDir));
    const sdkCtx = createSdkContext(projectB);
    const expectedFile = join(projectA, "src", "lib.rs");
    const deadline = Date.now() + 20_000;
    let output = "";
    let lastError: unknown;
    while (Date.now() < deadline) {
      try {
        output = await tools.aft_search.execute(
          { query: "handle_request", hint: "literal", path: projectA },
          sdkCtx,
        );
        if (output.includes(expectedFile)) break;
      } catch (error) {
        lastError = error;
      }
      await new Promise((resolve) => setTimeout(resolve, 250));
    }

    if (!output.includes(expectedFile) && lastError) throw lastError;
    expect(output).toContain(expectedFile);
    expect(output).toContain("lexical match");
  }, 40_000);
});

async function createFixtureProject(root: string): Promise<void> {
  await mkdir(join(root, "src"), { recursive: true });
  await Promise.all([
    writeFile(
      join(root, "src", "lib.rs"),
      [
        "pub fn handle_request(token: &str) -> bool {",
        "  !token.is_empty()",
        "}",
        "",
        "pub struct AuthService;",
        "",
      ].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "src", "utils.rs"),
      [
        "pub fn normalize_user_id(input: &str) -> String {",
        "  input.trim().to_lowercase()",
        "}",
        "",
      ].join("\n"),
      "utf8",
    ),
  ]);
}

function initGit(root: string): void {
  const options = { cwd: root, env: withHermeticGitEnv(), stdio: "ignore" as const };
  execFileSync("git", ["init"], options);
  execFileSync("git", ["config", "user.email", "test@example.com"], options);
  execFileSync("git", ["config", "user.name", "AFT Test"], options);
  execFileSync("git", ["add", "."], options);
  execFileSync("git", ["commit", "--no-gpg-sign", "-m", "initial"], options);
}
