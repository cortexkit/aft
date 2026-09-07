import { describe, expect, test } from "bun:test";

import type { AftStatusSnapshot } from "../../src/shared/status.js";
import { formatAftStatusSegment, summarizeAftSidebar } from "../../src/tui/v2-status.js";

function snapshot(): AftStatusSnapshot {
  return {
    version: "0.55.1",
    project_root: "/work/project",
    canonical_root: "/work/project",
    cache_role: "main",
    degraded: false,
    degraded_reasons: [],
    features: {
      format_on_edit: true,
      validate_on_edit: "syntax",
      restrict_to_project_root: true,
      search_index: true,
      semantic_search: true,
      callgraph_store: true,
    },
    search_index: { status: "ready", files: 12, trigrams: 34 },
    semantic_index: {
      status: "loading",
      stage: "embedding",
      refreshing_count: 0,
      entries: 4,
      dimension: 384,
    },
    disk: { storage_dir: "/cache", trigram_disk_bytes: 1, semantic_disk_bytes: 2 },
    lsp_servers: 0,
    runtime: { live_watchers: 1, live_actor_roots: 1, open_routes: 1 },
    symbol_cache: { local_entries: 1, warm_entries: 2 },
    storage_dir: "/cache",
    checkpoints_total: 0,
    session: { id: "ses_1", tracked_files: 0, checkpoints: 0 },
    status_bar: {
      errors: 1,
      warnings: 2,
      dead_code: 3,
      unused_exports: 4,
      duplicates: 5,
      todos: 6,
      tier2_stale: true,
    },
    message: "",
  };
}

describe("V2 TUI status presentation", () => {
  test("renders the fleet-compatible AFT footer segment", () => {
    expect(formatAftStatusSegment(snapshot())).toBe("AFT E1 W2 | ~D3 U4 C5 | T6");
  });

  test("does not turn unproven health categories into clean zeroes", () => {
    const status = snapshot();
    status.status_bar = { warnings: 2 };
    expect(formatAftStatusSegment(status)).toBe("AFT E? W2 | D? U? C? | T?");
  });

  test("summarizes only the status data needed by the V2 sidebar slot", () => {
    expect(summarizeAftSidebar(snapshot())).toEqual({
      title: "AFT",
      version: "0.55.1",
      search: "ready",
      semantic: "loading (embedding)",
      health: "E1 W2 ~D3 U4 C5 T6",
    });
  });
});
