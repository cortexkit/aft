import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  coerceAftStatus,
  formatCacheRoleLabel,
  formatSemanticIndexStatus,
  formatStatusDialogMessage,
  formatStatusMarkdown,
  semanticIndexStatusKind,
  worktreeCacheRoleNote,
} from "../shared/status.js";

const baseResponse = Object.freeze({
  version: "0.0.0-test",
  project_root: "/tmp/project",
  features: {
    format_on_edit: false,
    validate_on_edit: "off",
    restrict_to_project_root: false,
    search_index: true,
    semantic_search: true,
  },
  search_index: { status: "ready", files: 4, trigrams: 400 },
  semantic_index: {
    status: "ready",
    entries: 128,
    dimension: 384,
  },
  disk: {
    storage_dir: "/tmp/storage",
    trigram_disk_bytes: 1024,
    semantic_disk_bytes: 2048,
  },
  lsp_servers: 2,
  symbol_cache: { local_entries: 3, warm_entries: 6 },
  storage_dir: "/tmp/storage",
  semantic: {
    backend: "openai_compatible",
    model: "text-embedding-3-small",
    api_key_env: "AFT_SEMANTIC_KEY",
  },
});

describe("coerceAftStatus", () => {
  test("adds backend and model when provided", () => {
    const status = coerceAftStatus(baseResponse as unknown as Record<string, unknown>);

    expect(status.semantic_index.backend).toBe("openai_compatible");
    expect(status.semantic_index.model).toBe("text-embedding-3-small");
    expect(status.semantic_index).not.toHaveProperty("api_key_env");
  });

  test("opencode_status_snapshot_includes_compression_passthrough", () => {
    const status = coerceAftStatus({
      ...baseResponse,
      compression: {
        project: { events: 3, original_tokens: 300, compressed_tokens: 210, savings_tokens: 90 },
        session: { events: 1, original_tokens: 100, compressed_tokens: 70, savings_tokens: 30 },
      },
    } as unknown as Record<string, unknown>);

    expect(status.compression?.project.events).toBe(3);
    expect(status.compression?.session.savings_tokens).toBe(30);
  });

  test("parses status_bar when present", () => {
    const status = coerceAftStatus({
      ...baseResponse,
      status_bar: {
        errors: 7,
        warnings: 13,
        dead_code: 334,
        unused_exports: 222,
        duplicates: 1167,
        todos: 5,
        tier2_stale: true,
      },
    } as unknown as Record<string, unknown>);

    expect(status.status_bar?.errors).toBe(7);
    expect(status.status_bar?.duplicates).toBe(1167);
    expect(status.status_bar?.tier2_stale).toBe(true);
  });

  test("home-root degradation renders as disabled heavy indexes", () => {
    const status = coerceAftStatus({
      ...baseResponse,
      degraded: true,
      degraded_reasons: ["home_root"],
      features: { ...baseResponse.features, callgraph_store: false },
    } as unknown as Record<string, unknown>);

    expect(formatStatusDialogMessage(status)).toContain(
      "project root is your home directory; heavy indexes are disabled",
    );
    expect(formatStatusMarkdown(status)).toContain(
      "project root is your home directory; heavy indexes are disabled",
    );
    expect(formatStatusDialogMessage(status)).toContain("callgraph_store: disabled");
  });

  test("status_bar is undefined when null (Tier-2 not populated)", () => {
    const status = coerceAftStatus({
      ...baseResponse,
      status_bar: null,
    } as unknown as Record<string, unknown>);
    expect(status.status_bar).toBeUndefined();
  });

  test("omits unproven health categories instead of coercing them to zero", () => {
    const status = coerceAftStatus({
      ...baseResponse,
      status_bar: { errors: 7 },
    } as unknown as Record<string, unknown>);

    expect(status.status_bar).toEqual({ errors: 7 });
    const dialog = formatStatusDialogMessage(status);
    const markdown = formatStatusMarkdown(status);
    expect(dialog).toContain("- errors: 7");
    expect(dialog).not.toContain("- warnings:");
    expect(markdown).toContain("- **Errors:** 7");
    expect(markdown).not.toContain("- **Warnings:**");
  });
});

describe("formatStatus* output", () => {
  test("formats backend and model without leaking api key", () => {
    const status = coerceAftStatus(baseResponse as unknown as Record<string, unknown>);
    const dialog = formatStatusDialogMessage(status);
    const markdown = formatStatusMarkdown(status);

    expect(dialog).toContain("backend: openai_compatible");
    expect(dialog).toContain("model: text-embedding-3-small");
    expect(markdown).toContain("**Backend:** openai_compatible");
    expect(markdown).toContain("**Model:** text-embedding-3-small");
    expect(dialog).not.toContain("AFT_SEMANTIC_KEY");
    expect(markdown).not.toContain("AFT_SEMANTIC_KEY");
  });

  test("worktree cache role uses shared-index phrasing, not degraded", () => {
    expect(formatCacheRoleLabel("worktree")).toBe(
      "worktree — shared repo index (built by the main checkout)",
    );
    expect(formatCacheRoleLabel("read_only")).toBe(
      "read_only — sharing the repo index family (read-only borrow)",
    );
    expect(formatCacheRoleLabel("main")).toBe("main");
    expect(worktreeCacheRoleNote("worktree")).toBe(
      "shared repo index (built by the main checkout)",
    );
    expect(worktreeCacheRoleNote("main")).toBeNull();
    expect(worktreeCacheRoleNote("read_only")).toBeNull();

    const status = coerceAftStatus({
      ...baseResponse,
      cache_role: "worktree",
    } as unknown as Record<string, unknown>);
    const dialog = formatStatusDialogMessage(status);
    const markdown = formatStatusMarkdown(status);
    expect(dialog).toContain(
      "Cache role: worktree — shared repo index (built by the main checkout)",
    );
    expect(markdown).toContain("shared repo index (built by the main checkout)");
    expect(dialog.toLowerCase()).not.toContain("degraded");
    expect(markdown.toLowerCase()).not.toContain("degraded");
  });
});

/**
 * The daemon's own semantic-index vocabulary and the prefix it puts on every
 * missing-runtime message, read from the source that defines them. Parsed
 * rather than copied so a word added on the daemon side turns into a failing
 * test here instead of an unexplained word in the sidebar.
 */
const daemonSemanticIndexSource = join(
  import.meta.dir,
  "../../../../crates/aft/src/semantic_index.rs",
);

function readDaemonSource(): string {
  const source = readFileSync(daemonSemanticIndexSource, "utf8");
  if (source.length === 0) {
    throw new Error(`empty daemon source at ${daemonSemanticIndexSource}`);
  }
  return source;
}

function daemonSemanticStatusWords(): string[] {
  const source = readDaemonSource();
  const declaration = source.indexOf("pub const SEMANTIC_INDEX_STATUS_WORDS");
  const open = source.indexOf("&[", declaration);
  const close = source.indexOf("];", open);
  if (declaration < 0 || open < 0 || close < 0) {
    throw new Error("SEMANTIC_INDEX_STATUS_WORDS not found in the daemon source");
  }
  const words = [...source.slice(open, close).matchAll(/"([a-z_]+)"/g)].map((match) => match[1]);
  // The daemon lists ready/building/failed and several more; a handful of
  // matches means the parse drifted, and an empty list would make the coverage
  // assertion below pass without checking anything.
  if (words.length < 5) {
    throw new Error(`parsed only ${words.length} daemon status word(s): ${words.join(", ")}`);
  }
  return words;
}

function daemonMissingRuntimePrefix(): string {
  const match = readDaemonSource().match(/ONNX_RUNTIME_MISSING_PREFIX: &str = "([^"]+)"/);
  if (!match) {
    throw new Error("ONNX_RUNTIME_MISSING_PREFIX not found in the daemon source");
  }
  return match[1];
}

describe("formatSemanticIndexStatus", () => {
  test("a model change still reports as a rebuild", () => {
    // The rebuild wording is correct when a rebuild is really running; the
    // failure handling below must not cost us this case.
    expect(formatSemanticIndexStatus("building", "fingerprint_change")).toBe(
      "Rebuilding (model changed)",
    );
    expect(formatSemanticIndexStatus("loading", "fingerprint_change")).toBe(
      "Rebuilding (model changed)",
    );
  });

  test("a failed index is never reported as a rebuild", () => {
    // A build that dies leaves its stage behind. Reading the stage alone turns
    // a dead attempt into "Rebuilding (model changed)" and tells the user to
    // wait for a build that stopped.
    const label = formatSemanticIndexStatus(
      "building",
      "fingerprint_change",
      `${daemonMissingRuntimePrefix()} dlopen('libonnxruntime.dylib') failed: image not found`,
    );

    expect(label).not.toBe("Rebuilding (model changed)");
    expect(label).toContain("ONNX Runtime");
  });

  test("names the missing runtime and the command that installs it", () => {
    const label = formatSemanticIndexStatus(
      "failed",
      null,
      `${daemonMissingRuntimePrefix()} Run \`npx @cortexkit/aft doctor --fix\``,
    );

    expect(label).toContain("ONNX Runtime");
    expect(label).toContain("doctor --fix");
    // No platform-specific install advice: whether AFT can download the runtime
    // here is answered by the downloader, and `doctor --fix` is what asks it.
    expect(label).not.toContain("brew");
    expect(label).not.toContain("apt");
  });

  test("a missing runtime carried in the build stage is not progress", () => {
    const label = formatSemanticIndexStatus(
      "loading",
      `waiting_for_embedding_backend: ${daemonMissingRuntimePrefix()} dlopen failed`,
    );

    expect(label).not.toBe("loading");
    expect(label).toContain("ONNX Runtime");
  });

  test("an ordinary failure keeps its word and gains no progress wording", () => {
    expect(formatSemanticIndexStatus("failed", "fingerprint_change")).toBe("failed");
    expect(formatSemanticIndexStatus("backend_unavailable", null)).toBe("backend unavailable");
    expect(formatSemanticIndexStatus("ready", null)).toBe("ready");
  });

  test("classifies every status word the daemon can emit", () => {
    const unrenderable = daemonSemanticStatusWords().filter(
      (word) => semanticIndexStatusKind(word) === "unrecognized",
    );

    expect(unrenderable).toEqual([]);
  });

  test("no daemon status word is classified as progress and failure at once", () => {
    // The two kinds decide opposite advice (wait vs act), so the sets that
    // define them must not overlap.
    const progress = daemonSemanticStatusWords().filter(
      (word) => semanticIndexStatusKind(word) === "progress",
    );

    expect(progress).toEqual(["building", "loading"]);
  });
});
