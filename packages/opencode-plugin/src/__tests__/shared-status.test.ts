import { describe, expect, test } from "bun:test";
import {
  daemonMissingRuntimePrefix,
  daemonSemanticStatusWords,
} from "../../../aft-bridge/src/__tests__/test-utils/daemon-status-words.js";
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

  test("reads per-category status_bar_values, dropping null categories", () => {
    const status = coerceAftStatus({
      ...baseResponse,
      status_bar: null,
      status_bar_values: {
        errors: 0,
        warnings: null,
        dead_code: null,
        unused_exports: 3,
        duplicates: 2,
        todos: 1,
        tier2_stale: false,
      },
    } as unknown as Record<string, unknown>);
    expect(status.status_bar).toBeUndefined();
    expect(status.status_bar_values).toEqual({
      errors: 0,
      unused_exports: 3,
      duplicates: 2,
      todos: 1,
    });
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
 * Both plugin hosts render the daemon's semantic status through one
 * implementation in @cortexkit/aft-bridge; its own tests cover the formatting
 * rules. What this harness has to prove is that the implementation it imports
 * is that one — if this plugin ever grows a private copy again, the copy is
 * what these assertions run against, and a copy that has not kept up fails
 * here.
 */
describe("semantic index status as this harness imports it", () => {
  test("classifies every status word the daemon can emit", () => {
    const unrenderable = daemonSemanticStatusWords().filter(
      (word) => semanticIndexStatusKind(word) === "unrecognized",
    );

    expect(unrenderable).toEqual([]);
  });

  test("a dead index is not rendered as a rebuild", () => {
    const label = formatSemanticIndexStatus(
      "building",
      "fingerprint_change",
      `${daemonMissingRuntimePrefix()} dlopen('libonnxruntime.dylib') failed: image not found`,
    );

    expect(label).not.toBe("Rebuilding (model changed)");
    expect(label).toContain("ONNX Runtime");
  });

  test("a backend outage reads as words rather than a wire token", () => {
    expect(formatSemanticIndexStatus("backend_unavailable", null)).toBe("backend unavailable");
  });
});
