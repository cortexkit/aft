/**
 * AFT status dialog for Pi.
 *
 * Mirrors the @cortexkit/opencode-magic-context Pi `/ctx-status` pattern:
 * `ctx.ui.custom<undefined>(...)` opens an overlay backed by a Component
 * that implements its own render + handleInput. Unlike OpenCode's TUI
 * (which uses @opentui/solid with flex primitives), Pi's TUI is
 * line-oriented and monospace — so column alignment via `padEnd` IS the
 * idiomatic approach here, and looks correct because the host renders
 * lines in a fixed-width font.
 *
 * Refresh cadence (REFRESH_INTERVAL_MS) is short enough that status
 * transitions like "loading → ready" surface live in the dialog without
 * the user needing to close and re-open it.
 */

import { compressionSavingsPercent } from "@cortexkit/aft-bridge";
import type { ExtensionAPI, ExtensionCommandContext, Theme } from "@earendil-works/pi-coding-agent";
import {
  type Component,
  matchesKey,
  type TUI,
  truncateToWidth,
  visibleWidth,
} from "@earendil-works/pi-tui";

import packageJson from "../../package.json";
import {
  type AftStatusSnapshot,
  coerceAftStatus,
  formatBytes,
  type StatusCompression,
  type StatusCompressionAggregate,
} from "../shared/status.js";
import { bridgeFor, callBridge } from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

const REFRESH_INTERVAL_MS = 1500;
const OVERLAY_WIDTH = 84;

export async function showAftStatusDialog(
  pi: ExtensionAPI,
  extCtx: ExtensionCommandContext,
  pluginCtx: PluginContext,
): Promise<void> {
  await extCtx.ui.custom<undefined>(
    (tui, theme, _keybindings, done) =>
      new AftStatusDialogComponent({
        pi,
        extCtx,
        pluginCtx,
        theme,
        tui,
        done,
      }),
    {
      overlay: true,
      overlayOptions: { anchor: "center", width: OVERLAY_WIDTH },
    },
  );
}

interface DialogProps {
  pi: ExtensionAPI;
  extCtx: ExtensionCommandContext;
  pluginCtx: PluginContext;
  theme: Theme;
  tui: TUI;
  done: (value: undefined) => void;
}

class AftStatusDialogComponent implements Component {
  private readonly props: DialogProps;
  private snapshot: AftStatusSnapshot | null = null;
  private errorMessage: string | null = null;
  private refreshTimer: ReturnType<typeof setInterval> | null = null;
  private closed = false;

  constructor(props: DialogProps) {
    this.props = props;
    // Initial fetch synchronously kicks off — render once before the
    // promise resolves so the dialog shows a "connecting" placeholder
    // immediately. When the response lands we call requestRender() to
    // repaint with real data.
    void this.fetchOnce();
    this.refreshTimer = setInterval(() => {
      if (this.closed) return;
      void this.fetchOnce();
    }, REFRESH_INTERVAL_MS);
  }

  private async fetchOnce(): Promise<void> {
    try {
      const bridge = bridgeFor(this.props.pluginCtx, this.props.extCtx.cwd);
      // Prefer the in-memory push-frame cache — that's the whole reason
      // the v0.24 status-push pipeline exists. Only fall through to the
      // bridge RPC when the cache is empty (cold path on first call).
      const cached = bridge.getCachedStatus();
      const response = cached
        ? { success: true, ...cached }
        : await callBridge(bridge, "status", {}, this.props.extCtx);
      if (!cached) {
        bridge.cacheStatusSnapshot(response);
      }
      if (this.closed) return;
      this.snapshot = coerceAftStatus(response as Record<string, unknown>);
      this.errorMessage = null;
      this.props.tui.requestRender();
    } catch (err) {
      if (this.closed) return;
      this.errorMessage = err instanceof Error ? err.message : String(err);
      this.props.tui.requestRender();
    }
  }

  handleInput(data: string): void {
    if (matchesKey(data, "escape") || matchesKey(data, "ctrl+c") || matchesKey(data, "return")) {
      this.close();
    }
  }

  private close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.refreshTimer) {
      clearInterval(this.refreshTimer);
      this.refreshTimer = null;
    }
    this.props.done(undefined);
  }

  invalidate(): void {
    // stateless render — nothing to invalidate
  }

  render(width: number): string[] {
    // drawBorder reserves 2 border chars + 1 padding each side
    const innerWidth = Math.max(40, width - 4);
    const inner = renderInner(this.snapshot, this.errorMessage, this.props.theme, innerWidth);
    return drawBorder(inner, width, this.props.theme);
  }

  dispose(): void {
    if (this.refreshTimer) {
      clearInterval(this.refreshTimer);
      this.refreshTimer = null;
    }
  }
}

function renderInner(
  s: AftStatusSnapshot | null,
  error: string | null,
  theme: Theme,
  innerWidth: number,
): string[] {
  const lines: string[] = [];

  // Title
  lines.push(
    `${theme.fg("accent", theme.bold("⚡ AFT Status"))}   ${theme.fg(
      "muted",
      `v${s?.version ?? packageJson.version}`,
    )}`,
  );
  lines.push("");

  if (error && !s) {
    lines.push(theme.fg("warning", error));
    lines.push("");
    lines.push(theme.fg("muted", "Press Escape to close"));
    return lines;
  }
  if (!s) {
    lines.push(theme.fg("muted", "Connecting to AFT…"));
    return lines;
  }

  // Header — paths span full width. truncate to inner width if too long
  // so they don't blow out the border. Keep label/value separated by
  // padded label so paths visually align across rows.
  lines.push(rowFull("Project root", s.project_root ?? "(not configured)", theme, innerWidth));
  lines.push(rowFull("Canonical root", s.canonical_root ?? "(not configured)", theme, innerWidth));
  const cacheTone: ToneColor =
    s.cache_role === "main" ? "accent" : s.cache_role === "worktree" ? "warning" : "muted";
  lines.push(rowFull("Cache role", theme.fg(cacheTone, s.cache_role), theme, innerWidth));
  lines.push("");

  // Two-column body — Pi's TUI is monospace, so padEnd-based columns
  // render correctly here (unlike the OpenCode TUI which is proportional).
  const colWidth = Math.floor((innerWidth - 2) / 2);
  const left: string[] = [];
  const right: string[] = [];

  // Left: search index
  left.push(theme.fg("muted", "Search index"));
  left.push(kv("status", colorStatus(s.search_index.status, theme), theme));
  left.push(kv("files", formatCountShort(s.search_index.files), theme));
  left.push(kv("trigrams", formatCountShort(s.search_index.trigrams), theme));
  left.push(kv("disk", formatBytes(s.disk.trigram_disk_bytes), theme));

  // Right: semantic index
  right.push(theme.fg("muted", "Semantic index"));
  right.push(kv("status", colorStatus(s.semantic_index.status, theme), theme));
  right.push(kv("entries", formatCountShort(s.semantic_index.entries), theme));
  if (s.semantic_index.backend) right.push(kv("backend", s.semantic_index.backend, theme));
  if (s.semantic_index.model) right.push(kv("model", s.semantic_index.model, theme));
  if (s.semantic_index.dimension != null) {
    right.push(kv("dimension", String(s.semantic_index.dimension), theme));
  }
  right.push(kv("disk", formatBytes(s.disk.semantic_disk_bytes), theme));

  for (const line of renderColumns(left, right, colWidth)) lines.push(line);
  lines.push("");

  // Runtime + current session row
  const left2: string[] = [];
  const right2: string[] = [];

  left2.push(theme.fg("muted", "Runtime"));
  left2.push(kv("LSP servers", String(s.lsp_servers), theme));
  left2.push(
    kv(
      "symbol cache",
      `${formatCountShort(s.symbol_cache.local_entries)} local · ${formatCountShort(s.symbol_cache.warm_entries)} warm`,
      theme,
    ),
  );

  right2.push(theme.fg("muted", "Current session"));
  right2.push(kv("tracked files", String(s.session.tracked_files), theme));
  right2.push(kv("checkpoints", String(s.session.checkpoints), theme));
  right2.push(kv("all-session", String(s.checkpoints_total), theme));

  for (const line of renderColumns(left2, right2, colWidth)) lines.push(line);
  lines.push("");

  // Features
  lines.push(theme.fg("muted", "Features"));
  lines.push(
    `  ${featureBadge("format_on_edit", s.features.format_on_edit, theme)}  ${featureBadge("search_index", s.features.search_index, theme)}  ${featureBadge("semantic_search", s.features.semantic_search, theme)}`,
  );

  const compressionRows = formatCompressionStatusRows(s.compression);
  if (compressionRows.length > 0) {
    lines.push("");
    lines.push(theme.fg("muted", "Compression"));
    // Each row already contains its own indentation (scope headers are
    // flush-left, stat rows are indented by 2). Don't double-prefix.
    for (const row of compressionRows) lines.push(row);
  }

  // Optional semantic build progress
  if (s.semantic_index.stage) {
    lines.push("");
    lines.push(theme.fg("muted", "Semantic build progress"));
    lines.push(kv("stage", s.semantic_index.stage, theme));
    if (s.semantic_index.files != null) {
      lines.push(kv("files seen", formatCountShort(s.semantic_index.files), theme));
    }
    if (s.semantic_index.entries_done != null || s.semantic_index.entries_total != null) {
      lines.push(
        kv(
          "progress",
          `${formatCountShort(s.semantic_index.entries_done ?? null)} / ${formatCountShort(s.semantic_index.entries_total ?? null)}`,
          theme,
        ),
      );
    }
  }

  // Errors — full width, error color
  if (s.semantic_index.error) {
    lines.push("");
    lines.push(theme.fg("error", `⚠ ${s.semantic_index.error}`));
  }
  if (error) {
    lines.push("");
    lines.push(theme.fg("warning", `⚠ ${error}`));
  }

  lines.push("");
  lines.push(theme.fg("muted", "Press Escape to close"));
  return lines;
}

export function renderStatusDialogInnerForTest(
  s: AftStatusSnapshot | null,
  error: string | null,
  theme: Theme,
  innerWidth: number,
): string[] {
  return renderInner(s, error, theme, innerWidth);
}

function appendCompressionScope(
  rows: string[],
  label: string,
  aggregate: StatusCompressionAggregate,
): void {
  const pct = compressionSavingsPercent(aggregate.original_tokens, aggregate.compressed_tokens);
  const savings = aggregate.savings_tokens;
  // Tabular layout matching OpenCode's sidebar/dialog: scope header line
  // followed by two stat lines (Tokens Saved + Compression Ratio). Pi's TUI
  // is monospace, so the kv() helper provides column alignment via the
  // outer renderInner pipeline.
  rows.push(label);
  rows.push(`  Tokens Saved        ${savings.toLocaleString("en-US")}`);
  rows.push(`  Compression Ratio   ${pct ?? 0}%`);
}

export function formatCompressionStatusRows(compression: StatusCompression | undefined): string[] {
  if (!compression || compression.project.events <= 0) return [];

  const rows: string[] = [];
  if (compression.session.events > 0) {
    appendCompressionScope(rows, "Session", compression.session);
  }
  appendCompressionScope(rows, "Project", compression.project);
  return rows;
}

type ToneColor = "accent" | "warning" | "error" | "muted" | "success";

function colorStatus(status: string, theme: Theme): string {
  switch (status) {
    case "ready":
      // some themes have "success", fall back to accent
      try {
        return theme.fg("success", status);
      } catch {
        return theme.fg("accent", status);
      }
    case "loading":
    case "building":
      return theme.fg("warning", status);
    case "failed":
    case "error":
      return theme.fg("error", status);
    case "disabled":
      return theme.fg("muted", status);
    default:
      return status;
  }
}

function featureBadge(name: string, enabled: boolean, theme: Theme): string {
  const indicator = enabled ? theme.fg("accent", "●") : theme.fg("muted", "○");
  const label = enabled ? name : theme.fg("muted", name);
  return `${indicator} ${label}`;
}

function kv(label: string, value: string, theme: Theme): string {
  return `  ${theme.fg("muted", `${label}:`)} ${value}`;
}

function rowFull(label: string, value: string, theme: Theme, innerWidth: number): string {
  const labelText = `${label}: `;
  const remaining = Math.max(10, innerWidth - visibleWidth(labelText));
  const truncated = truncateToWidth(value, remaining, "…");
  return `${theme.fg("muted", labelText)}${truncated}`;
}

/**
 * Pair two arrays of lines into a two-column layout. Each row is the
 * left line padded to `colWidth` (by visible width, ignoring ANSI escape
 * sequences) + a 2-space gutter + the right line. Missing lines on
 * either side render as blanks.
 */
function renderColumns(left: string[], right: string[], colWidth: number): string[] {
  const rows = Math.max(left.length, right.length);
  const out: string[] = [];
  for (let i = 0; i < rows; i++) {
    const l = left[i] ?? "";
    const r = right[i] ?? "";
    const visible = visibleWidth(l);
    const pad = " ".repeat(Math.max(0, colWidth - visible));
    out.push(`${l}${pad}  ${r}`);
  }
  return out;
}

function drawBorder(inner: string[], width: number, theme: Theme): string[] {
  const innerWidth = Math.max(40, width - 4);
  const border = (s: string) => theme.fg("borderMuted", s);

  const top = border(`╭${"─".repeat(innerWidth + 2)}╮`);
  const bottom = border(`╰${"─".repeat(innerWidth + 2)}╯`);
  const side = border("│");

  const out: string[] = [];
  out.push(top);
  for (const raw of inner) {
    const line = truncateToWidth(raw, innerWidth, "…");
    const visible = visibleWidth(line);
    const pad = " ".repeat(Math.max(0, innerWidth - visible));
    out.push(`${side} ${line}${pad} ${side}`);
  }
  out.push(bottom);
  return out;
}

function formatCountShort(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${Math.round(value / 1_000)}K`;
  return String(value);
}
