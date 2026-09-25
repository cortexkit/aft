/**
 * Unit tests for shared AFT status response shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  daemonMissingRuntimePrefix,
  daemonSemanticStatusWords,
} from "../../../aft-bridge/src/__tests__/test-utils/daemon-status-words.js";
import {
  coerceAftStatus,
  formatBytes,
  formatCacheRoleLabel,
  formatSemanticIndexLabel,
  formatSemanticIndexStatus,
  formatStatusDialogMessage,
  formatStatusMarkdown,
  semanticIndexStatusKind,
} from "../shared/status.js";

describe("shared status helpers", () => {
  test("coerceAftStatus tolerates missing and malformed fields without crashing", () => {
    const status = coerceAftStatus({
      version: 123,
      features: { experimental_search_index: true, semantic_search: "yes" },
      search_index: { files: Number.NaN },
      semantic_config: { backend: "openai", model: "text-embedding-3-small" },
      disk: { trigram_disk_bytes: "large", semantic_disk_bytes: 1536 },
      symbol_cache: { local_entries: 2, warm_entries: Infinity },
    });

    expect(status.version).toBe("unknown");
    expect(status.features.search_index).toBe(true);
    expect(status.features.semantic_search).toBe(false);
    expect(status.search_index.files).toBeNull();
    expect(status.semantic_index.backend).toBe("openai");
    expect(status.semantic_index.model).toBe("text-embedding-3-small");
    expect(status.disk.trigram_disk_bytes).toBe(0);
    expect(status.disk.semantic_disk_bytes).toBe(1536);
    expect(status.symbol_cache.warm_entries).toBe(0);
  });

  test("home-root degradation renders as disabled heavy indexes", () => {
    const status = coerceAftStatus({
      degraded: true,
      degraded_reasons: ["home_root"],
      features: { callgraph_store: false },
    });

    expect(formatStatusDialogMessage(status)).toContain(
      "project root is your home directory; heavy indexes are disabled",
    );
    expect(formatStatusMarkdown(status)).toContain(
      "project root is your home directory; heavy indexes are disabled",
    );
    expect(formatStatusDialogMessage(status)).toContain("callgraph_store: disabled");
  });

  test("pi_status_snapshot_includes_compression_passthrough", () => {
    const status = coerceAftStatus({
      compression: {
        project: { events: 3, original_tokens: 300, compressed_tokens: 210, savings_tokens: 90 },
        session: { events: 1, original_tokens: 100, compressed_tokens: 70, savings_tokens: 30 },
      },
    });

    expect(status.compression?.project.events).toBe(3);
    expect(status.compression?.session.savings_tokens).toBe(30);
  });

  test("pi_status_snapshot_parses_status_bar_and_renders_code_health", () => {
    const status = coerceAftStatus({
      status_bar: {
        errors: 7,
        warnings: 13,
        dead_code: 334,
        unused_exports: 222,
        duplicates: 1167,
        todos: 5,
        tier2_stale: true,
      },
    });

    expect(status.status_bar?.errors).toBe(7);
    expect(status.status_bar?.duplicates).toBe(1167);
    expect(status.status_bar?.tier2_stale).toBe(true);

    const dialog = formatStatusDialogMessage(status);
    expect(dialog).toContain("Code Health (~ stale)");
    expect(dialog).toContain("duplicates: 1,167");
    expect(status.status_bar?.dead_code).toBe(334);
    expect(dialog).toContain("dead code: 334");
    expect(dialog).toContain("unused exports: 222");
  });

  test("pi_status_snapshot_status_bar_undefined_when_null", () => {
    const status = coerceAftStatus({ status_bar: null });
    expect(status.status_bar).toBeUndefined();
    expect(formatStatusDialogMessage(status)).not.toContain("Code Health");
  });

  test("pi_status_omits_unproven_health_categories_instead_of_coercing_zero", () => {
    const status = coerceAftStatus({ status_bar: { errors: 7 } });

    expect(status.status_bar).toEqual({ errors: 7 });
    const dialog = formatStatusDialogMessage(status);
    const markdown = formatStatusMarkdown(status);
    expect(dialog).toContain("- errors: 7");
    expect(dialog).not.toContain("- warnings:");
    expect(markdown).toContain("- **Errors:** 7");
    expect(markdown).not.toContain("- **Warnings:**");
  });

  test("formatBytes handles zero, fractions, and large units", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(10 * 1024 * 1024)).toBe("10 MB");
  });

  test("dialog and markdown include semantic progress, errors, and storage dir", () => {
    const status = coerceAftStatus({
      version: "0.19.0",
      project_root: "/repo",
      features: { format_on_edit: true, search_index: true, semantic_search: true },
      search_index: { status: "ready", files: 1000, trigrams: 2000 },
      semantic_index: {
        status: "indexing",
        entries: 50,
        entries_done: 10,
        entries_total: 50,
        stage: "embedding",
        error: "rate limited",
      },
      disk: { storage_dir: "/tmp/aft", trigram_disk_bytes: 1024, semantic_disk_bytes: 2048 },
      lsp_servers: 3,
      symbol_cache: { local_entries: 4, warm_entries: 5 },
    });

    const dialog = formatStatusDialogMessage(status);
    const markdown = formatStatusMarkdown(status);

    expect(dialog).toContain("semantic progress: 10 / 50");
    expect(dialog).toContain("Semantic error\nrate limited");
    expect(markdown).toContain("**Progress:** 10 / 50");
    expect(markdown).toContain("**Storage dir:** `/tmp/aft`");
  });

  test("worktree cache role uses shared-index phrasing, not degraded", () => {
    expect(formatCacheRoleLabel("worktree")).toBe(
      "worktree — shared repo index (built by the main checkout)",
    );
    expect(formatCacheRoleLabel("read_only")).toBe(
      "read_only — sharing the repo index family (read-only borrow)",
    );
    expect(formatCacheRoleLabel("main")).toBe("main");

    const status = coerceAftStatus({ cache_role: "worktree" });
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
 * is that one — this file used to hold a private copy that still rendered a
 * dead index as a rebuild, and a copy that has not kept up is what fails here.
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

  test("an unreachable backend names its URL and reason instead of loading", () => {
    const status = coerceAftStatus({
      semantic_index: {
        status: "backend_unavailable",
        reason: "connection refused",
        backend_url: "http://localhost:1234/v1",
      },
    });
    const expected = "backend unavailable (http://localhost:1234/v1): connection refused";

    expect(formatSemanticIndexLabel(status.semantic_index)).toBe(expected);
    expect(formatStatusDialogMessage(status)).toContain(`- status: ${expected}`);
    expect(formatStatusMarkdown(status)).toContain(expected);
  });

  test("the status dialog names a missing runtime instead of a build stage", () => {
    const status = coerceAftStatus({
      semantic_index: {
        status: "failed",
        stage: "fingerprint_change",
        error: `${daemonMissingRuntimePrefix()} Run \`npx @cortexkit/aft doctor --fix\``,
      },
    });

    const dialog = formatStatusDialogMessage(status);

    expect(dialog).toContain("ONNX Runtime missing");
    expect(dialog).not.toContain("- status: failed");
  });
});
