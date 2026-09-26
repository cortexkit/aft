import { describe, expect, test } from "bun:test";

import { type AftStatusSnapshot, coerceAftStatus } from "../../src/shared/status.js";
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

  test("claims startup only before a real snapshot has arrived", () => {
    expect(formatAftStatusSegment(null)).toBe("AFT starting…");
    const placeholder = snapshot();
    placeholder.cache_role = "not_initialized";
    placeholder.status_bar = undefined;
    expect(formatAftStatusSegment(placeholder)).toBe("AFT starting…");
  });

  test("shows known categories when one producer is unavailable", () => {
    // The bridge withholds the complete status_bar while dead-code has no
    // value (issue #334: callgraph unavailable), but still reports the rest.
    const status = snapshot();
    status.status_bar = undefined;
    status.status_bar_values = {
      errors: 0,
      warnings: 1,
      unused_exports: 4,
      duplicates: 5,
      todos: 6,
    };
    expect(formatAftStatusSegment(status)).toBe("AFT E0 W1 | D? U4 C5 | T6");
  });

  test("marks every category pending when a snapshot has no values yet", () => {
    const status = snapshot();
    status.status_bar = undefined;
    expect(formatAftStatusSegment(status)).toBe("AFT E? W? | D? U? C? | T?");
  });

  test("names a missing language server instead of leaving E and W pending", () => {
    // The snapshot a 1-file project with no language server produces: Tier-2
    // and todos counts resolved, errors and warnings never will.
    const status = coerceAftStatus({
      version: "0.58.0",
      cache_role: "main",
      status_bar: null,
      status_bar_values: {
        errors: null,
        warnings: null,
        diagnostics: "no_language_server",
        dead_code: 1,
        unused_exports: 1,
        duplicates: 0,
        todos: 0,
        tier2_stale: false,
      },
    });
    expect(formatAftStatusSegment(status)).toBe("AFT no LSP | D1 U1 C0 | T0");
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

  test("a Mac without developer tools is titled git features off, not DEGRADED", () => {
    const gitOff = {
      ...snapshot(),
      degraded: true,
      degraded_reasons: ["git features are off: macOS developer tools are not installed."],
    };
    expect(summarizeAftSidebar(gitOff).title).toBe("AFT · ⚠ git features off");
    const homeRoot = { ...snapshot(), degraded: true, degraded_reasons: ["home_root"] };
    expect(summarizeAftSidebar(homeRoot).title).toBe("AFT · DEGRADED");
  });
});
