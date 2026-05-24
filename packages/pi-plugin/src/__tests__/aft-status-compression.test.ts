/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { Theme } from "@earendil-works/pi-coding-agent";
import {
  formatCompressionStatusRows,
  renderStatusDialogInnerForTest,
} from "../dialogs/status-dialog.js";
import { coerceAftStatus } from "../shared/status.js";

const theme = {
  bold: (text: string) => text,
  fg: (_color: string, text: string) => text,
} as unknown as Theme;

function status(compression: Record<string, unknown> | undefined) {
  return coerceAftStatus({
    version: "0.26.4",
    project_root: "/repo",
    canonical_root: "/repo",
    cache_role: "main",
    features: { format_on_edit: true, search_index: true, semantic_search: true },
    search_index: { status: "ready", files: 10, trigrams: 20 },
    semantic_index: { status: "ready", entries: 30 },
    disk: { trigram_disk_bytes: 1024, semantic_disk_bytes: 2048 },
    lsp_servers: 1,
    symbol_cache: { local_entries: 2, warm_entries: 3 },
    session: { id: "s1", tracked_files: 4, checkpoints: 5 },
    checkpoints_total: 6,
    compression,
  });
}

describe("Pi aft-status compression rendering", () => {
  test("pi_status_renders_compression_when_project_events_present", () => {
    const lines = renderStatusDialogInnerForTest(
      status({
        project: {
          events: 1234,
          original_tokens: 567000,
          compressed_tokens: 234000,
          savings_tokens: 333000,
        },
        session: {
          events: 12,
          original_tokens: 5600,
          compressed_tokens: 2300,
          savings_tokens: 3300,
        },
      }),
      null,
      theme,
      80,
    );

    // Tabular format: scope header line ("Session"/"Project") followed by
    // two stat lines with Tokens Saved and Compression Ratio. Use array
    // `some` predicates so we tolerate any incidental ANSI/theme escapes
    // wrapped around the visible text. 333,000 / 567,000 ≈ 59%.
    expect(lines).toContain("Compression");
    expect(lines).toContain("Project");
    expect(lines.some((line) => line.includes("Tokens Saved") && line.includes("333,000"))).toBe(
      true,
    );
    expect(lines.some((line) => line.includes("Compression Ratio") && line.includes("59%"))).toBe(
      true,
    );
  });

  test("pi_status_hides_compression_when_zero_events", () => {
    const rows = formatCompressionStatusRows({
      project: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
      session: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
    });

    expect(rows).toEqual([]);
  });

  test("pi_status_renders_savings_delta_when_original_nonzero", () => {
    // 40 saved / 100 original = 40% reduction. Locale grouping omitted for
    // values < 1000 — "40" is the right formatting for a small count.
    const rows = formatCompressionStatusRows({
      project: { events: 1, original_tokens: 100, compressed_tokens: 60, savings_tokens: 40 },
      session: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
    });

    // Tabular format: scope header + two stat rows.
    expect(rows).toEqual(["Project", "  Tokens Saved        40", "  Compression Ratio   40%"]);
  });

  test("pi_status_renders_zero_savings_cleanly", () => {
    // A scope where no commands matched any compressor (savings_tokens === 0)
    // must render as "0 tokens, 0% reduction" rather than "-0 tokens, …" so
    // users don't read a noop as a regression.
    const rows = formatCompressionStatusRows({
      project: { events: 1, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
      session: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
    });

    expect(rows).toEqual(["Project", "  Tokens Saved        0", "  Compression Ratio   0%"]);
  });
});
