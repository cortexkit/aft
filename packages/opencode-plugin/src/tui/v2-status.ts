import type { AftStatusSnapshot, StatusBar } from "../shared/status";

function count(value: number | undefined): string {
  return value === undefined ? "?" : String(value);
}

export function formatAftStatusSegment(status: AftStatusSnapshot | null): string {
  const bar = status?.status_bar;
  if (!bar) return "AFT starting…";
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
