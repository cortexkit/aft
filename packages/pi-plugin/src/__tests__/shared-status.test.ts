/**
 * Unit tests for shared AFT status response shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  coerceAftStatus,
  formatBytes,
  formatStatusDialogMessage,
  formatStatusMarkdown,
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
});
