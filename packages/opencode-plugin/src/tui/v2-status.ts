import type { AftStatusSnapshot, StatusBar } from "../shared/status";

function count(value: number | undefined): string {
  return value === undefined ? "?" : String(value);
}

/**
 * The V2 footer segment. "AFT starting…" is only true before the bridge has
 * delivered a real snapshot. After that the footer shows every health category
 * that has a value and `?` for any that does not, because a producer that is
 * unavailable (for example dead-code when the callgraph store cannot be used)
 * can leave the complete `status_bar` missing for the whole session.
 */
export function formatAftStatusSegment(status: AftStatusSnapshot | null): string {
  // The config error state never produces a real snapshot; say so instead of
  // "starting", which would suggest the bridge is on its way.
  if (status?.config_error) return "AFT config error";
  if (!status || status.cache_role === "not_initialized") return "AFT starting…";
  const bar: StatusBar = status.status_bar ?? status.status_bar_values ?? {};
  const stale = bar.tier2_stale ? "~" : "";
  return (
    `AFT E${count(bar.errors)} W${count(bar.warnings)} | ` +
    `${stale}D${count(bar.dead_code)} U${count(bar.unused_exports)} ` +
    `C${count(bar.duplicates)} | T${count(bar.todos)}`
  );
}

export type AftSidebarSummary = {
  title: string;
  version?: string;
  search: string;
  semantic: string;
  health: string;
};
function healthSummary(bar: StatusBar | undefined): string {
  if (!bar) return "health pending";
  const stale = bar.tier2_stale ? "~" : "";
  return (
    `E${count(bar.errors)} W${count(bar.warnings)} ` +
    `${stale}D${count(bar.dead_code)} U${count(bar.unused_exports)} ` +
    `C${count(bar.duplicates)} T${count(bar.todos)}`
  );
}

/**
 * A one-line-per-field digest of the status snapshot.
 *
 * This is no longer what the sidebar draws: the sidebar now renders the shared
 * panel in ./sidebar-view, which labels each section and reuses the status
 * wording ./sidebar.tsx established. The digest is still pinned by
 * test/tui/v2-status.test.ts.
 */
export function summarizeAftSidebar(status: AftStatusSnapshot | null): AftSidebarSummary {
  if (!status || status.cache_role === "not_initialized") {
    return {
      title: "AFT",
      search: "not initialized",
      semantic: "not initialized",
      health: "health pending",
    };
  }

  const semantic = status.semantic_index.stage
    ? `${status.semantic_index.status} (${status.semantic_index.stage})`
    : status.semantic_index.status;
  return {
    title: status.degraded ? "AFT · DEGRADED" : "AFT",
    version: status.version || undefined,
    search: status.search_index.status,
    semantic,
    health: healthSummary(status.status_bar),
  };
}
