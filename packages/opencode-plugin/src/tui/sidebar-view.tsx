/** @jsxImportSource @opentui/solid */
// @ts-nocheck

// The AFT sidebar panel, shared by both OpenCode hosts.
//
// Everything that decides what the panel *draws* lives here and takes the
// status snapshot, the user's preferences and a colour palette as plain
// inputs. The two hosts differ only in how a component is mounted and how the
// theme arrives, so those two things stay at the edge (sidebar.tsx for the
// slot-plugin host, v2.tsx for the GA host) and the body below is rendered
// identically by both. Without this split the second host drifts into a
// smaller, differently-worded panel, which is exactly what happened before.

import { createMemo, createSignal, onCleanup } from "solid-js";
import {
  type AftStatusSnapshot,
  formatSemanticIndexLabel,
  formatSemanticRefreshing,
  NOT_STARTED_STATUS_TEXT,
  type StatusBar,
  type StatusCompression,
  semanticIndexStatusKind,
  worktreeCacheRoleNote,
} from "../shared/status";
import { badgeTextColor } from "./badge-contrast";
import {
  type AftTuiPrefs,
  DEFAULT_PREFS,
  persistCollapsedIfEnabled,
  readTuiPreferencesFile,
  resolveAftPrefs,
  seedCollapsedFromPrefs,
  watchTuiPreferences,
} from "./preferences";

const SINGLE_BORDER = { type: "single" } as any;

/**
 * A colour is whatever the host's theme hands us: OpenTUI accepts both its own
 * RGBA instances and hex strings, and the two hosts spell their themes
 * differently, so the panel never looks inside a colour — it only passes it on.
 */
export type SidebarColor = unknown;

/**
 * The colours the panel draws with, named after their role rather than after
 * either host's theme keys. Each host resolves its own theme into this shape
 * before rendering; see resolveV1Palette / resolveV2Palette at the edges.
 */
export interface SidebarPalette {
  text: SidebarColor;
  textMuted: SidebarColor;
  success: SidebarColor;
  warning: SidebarColor;
  error: SidebarColor;
  accent: SidebarColor;
  background: SidebarColor;
  border: SidebarColor;
}

type Channels = { r: number; g: number; b: number; a?: number };

function hexChannels(value: string): Channels | null {
  const hex = value.trim().replace(/^#/, "");
  if (hex.length !== 3 && hex.length !== 6 && hex.length !== 8) return null;
  const wide = hex.length === 3 ? [...hex].map((c) => c + c).join("") : hex;
  const int = Number.parseInt(wide.slice(0, 6), 16);
  if (Number.isNaN(int)) return null;
  const alpha = wide.length === 8 ? Number.parseInt(wide.slice(6, 8), 16) / 255 : 1;
  return {
    r: ((int >> 16) & 0xff) / 255,
    g: ((int >> 8) & 0xff) / 255,
    b: (int & 0xff) / 255,
    a: alpha,
  };
}

/**
 * Read a colour's 0..1 channels for the badge contrast decision only. OpenTUI's
 * RGBA already exposes r/g/b/a getters in that range; a hex string has to be
 * parsed. Anything else returns null and the caller keeps its existing choice
 * rather than guessing.
 */
function colorChannels(value: SidebarColor): Channels | null {
  if (typeof value === "string") return hexChannels(value);
  if (value && typeof value === "object") {
    const candidate = value as Partial<Channels>;
    if (typeof candidate.r === "number" && typeof candidate.g === "number") {
      if (typeof candidate.b !== "number") return null;
      return { r: candidate.r, g: candidate.g, b: candidate.b, a: candidate.a };
    }
  }
  return null;
}

/**
 * Text colour for a label drawn on the accent badge. badgeTextColor works on
 * plain channels, but the value handed back to OpenTUI has to be the host's
 * original colour object, so the decision is made on the channels and then
 * mapped back.
 */
export function badgeForeground(accent: SidebarColor, background: SidebarColor): SidebarColor {
  const accentChannels = colorChannels(accent);
  const backgroundChannels = colorChannels(background);
  if (!accentChannels || !backgroundChannels) return background;
  const picked = badgeTextColor(accentChannels, backgroundChannels);
  return picked === backgroundChannels ? background : picked;
}

function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "—";
  if (n >= 1_073_741_824) return `${(n / 1_073_741_824).toFixed(1)} GB`;
  if (n >= 1_048_576) return `${(n / 1_048_576).toFixed(1)} MB`;
  if (n >= 1_024) return `${Math.round(n / 1_024)} KB`;
  return `${n} B`;
}

function formatCount(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "—";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}K`;
  return String(n);
}

/** Tagged rows for the Compression section. Each scope (Session / Project)
 * emits a "scope" header followed by two "stat" rows — Tokens Saved and
 * Compression Ratio — so the renderer can use the same StatRow layout as
 * Search Index / Semantic Index above. Pi's monospace overlay and the
 * OpenCode TUI dialog/sidebar all consume this same shape. */
export type CompressionRow =
  | { kind: "scope"; label: string }
  | { kind: "stat"; label: string; value: string };

function appendScope(
  rows: CompressionRow[],
  label: string,
  scope: {
    events: number;
    original_tokens: number;
    compressed_tokens: number;
    savings_tokens: number;
  },
): void {
  const savings = scope.savings_tokens;
  const pct = scope.original_tokens > 0 ? Math.round((savings / scope.original_tokens) * 100) : 0;
  rows.push({ kind: "scope", label });
  rows.push({ kind: "stat", label: "Tokens Saved", value: savings.toLocaleString("en-US") });
  rows.push({ kind: "stat", label: "Compression Ratio", value: `${pct}%` });
}

export function formatCompressionSidebarRows(
  compression: StatusCompression | undefined,
): CompressionRow[] {
  if (!compression || compression.project.events <= 0) return [];

  const rows: CompressionRow[] = [];
  if (compression.session.events > 0) {
    appendScope(rows, "Session", compression.session);
  }
  appendScope(rows, "Project", compression.project);

  return rows;
}

// Map an index status word to (label, theme color name). The label is what we
// want the user to see; the color encodes severity so the eye lands on trouble.
//
// The tone comes from the same classification the label uses
// (`semanticIndexStatusKind` in @cortexkit/aft-bridge), so a word cannot be
// readable in one column and unrecognised grey in the other.
// `backend_unavailable` used to fall through to the default arm: a real
// embedding-backend outage was drawn in the same muted grey as a word the
// renderer has no idea about, which reads as noise rather than as a condition
// the user has to act on.
export function statusDisplay(status: string): {
  label: string;
  tone: "ok" | "warn" | "err" | "muted";
} {
  const label = status || "unknown";
  switch (semanticIndexStatusKind(status)) {
    case "ready":
      return { label, tone: "ok" };
    case "progress":
      return { label, tone: "warn" };
    case "failure":
      return { label, tone: "err" };
    case "inactive":
      return { label, tone: "muted" };
    default:
      // Nothing is known about this word, so grey is honest here: it says the
      // renderer has no reading to offer, not that everything is fine.
      return { label, tone: "muted" };
  }
}

const StatRow = (props: {
  palette: SidebarPalette;
  label: string;
  value: string;
  tone?: "ok" | "warn" | "err" | "muted" | "accent";
}) => {
  const fg = createMemo(() => {
    switch (props.tone) {
      case "ok":
        return props.palette.success;
      case "warn":
        return props.palette.warning;
      case "err":
        return props.palette.error;
      case "muted":
        return props.palette.textMuted;
      case "accent":
        return props.palette.accent;
      default:
        return props.palette.text;
    }
  });

  return (
    <box width="100%" flexDirection="row" justifyContent="space-between">
      <text fg={props.palette.textMuted}>{props.label}</text>
      <text fg={fg()}>
        <b>{props.value}</b>
      </text>
    </box>
  );
};

const SectionHeader = (props: { palette: SidebarPalette; title: string; marginTop?: number }) => (
  <box width="100%" marginTop={props.marginTop ?? 1}>
    <text fg={props.palette.text}>
      <b>{props.title}</b>
    </text>
  </box>
);

// Map a status tone to a palette color — used for the collapsed-view status dots.
function toneColor(palette: SidebarPalette, tone: "ok" | "warn" | "err" | "muted"): SidebarColor {
  switch (tone) {
    case "ok":
      return palette.success;
    case "warn":
      return palette.warning;
    case "err":
      return palette.error;
    default:
      return palette.textMuted;
  }
}

// Collapsed-view row: label on the left, a status dot (or compact value) on the
// right. Mirrors the expanded StatRow layout so the columns line up.
const CollapsedRow = (props: { palette: SidebarPalette; label: string; children: unknown }) => (
  <box width="100%" flexDirection="row" justifyContent="space-between">
    <text fg={props.palette.textMuted}>{props.label}</text>
    {props.children}
  </box>
);

// Compact "saved / ratio" string for the collapsed Compression row — e.g.
// "7.6M / 64%". Uses the local `formatCount` (not the aft-bridge token
// formatter) so the TUI bundle doesn't pull the bridge barrel, which exports
// URL-fetch helpers unsuitable for Bun's TUI runtime. Returns null when no
// compression has been recorded yet.
export function collapsedCompressionValue(
  compression: StatusCompression | undefined,
): string | null {
  if (!compression || compression.project.events <= 0) return null;
  const { savings_tokens, original_tokens } = compression.project;
  const pct = original_tokens > 0 ? Math.round((savings_tokens / original_tokens) * 100) : 0;
  return `${formatCount(savings_tokens)} / ${pct}%`;
}

export type HealthLightTone = "ok" | "warn" | "err" | "muted";

// Degraded-mode reason → human-readable hint. Distinct strings per reason
// because the UX direction is different: "home_root" tells the user to open a
// real project subdirectory, "search_too_many_files" tells them the tree is too
// big for full indexing, and "watcher_unavailable" is an honest soft
// degradation (AFT continues without live external-change invalidation).
export function degradedReasonLabel(reason: string): string {
  if (reason === "home_root") {
    return "project root is your home directory";
  }
  if (reason.startsWith("search_too_many_files:")) {
    const threshold = reason.split(":")[1] ?? "20000";
    return `project exceeds ${threshold} files`;
  }
  if (reason === "watcher_unavailable") {
    return "file watcher unavailable; continuing without live external-change invalidation";
  }
  return reason; // unknown reason — surface verbatim so users can grep logs
}

export interface HealthLights {
  diagnostics: HealthLightTone;
  code: HealthLightTone;
  todos: HealthLightTone;
}

// Missing categories are intentionally muted: a green light requires an
// explicit zero for every category that feeds that light.
export function collapsedHealthLights(statusBar: StatusBar | undefined): HealthLights | null {
  if (!statusBar) return null;
  const diagnostics: HealthLightTone =
    statusBar.errors !== undefined && statusBar.errors > 0
      ? "err"
      : statusBar.warnings !== undefined && statusBar.warnings > 0
        ? "warn"
        : statusBar.errors === 0 && statusBar.warnings === 0
          ? "ok"
          : "muted";
  const codeValues = [statusBar.dead_code, statusBar.unused_exports, statusBar.duplicates];
  const code: HealthLightTone = codeValues.some((value) => value !== undefined && value > 0)
    ? "warn"
    : codeValues.every((value) => value === 0)
      ? "ok"
      : "muted";
  const todos: HealthLightTone =
    statusBar.todos === undefined ? "muted" : statusBar.todos > 0 ? "warn" : "ok";
  return { diagnostics, code, todos };
}

export interface AftSidebarPanelProps {
  palette: SidebarPalette;
  snapshot: AftStatusSnapshot | null;
  prefs: AftTuiPrefs;
  collapsed: boolean;
  onToggleCollapsed: () => void;
  pluginVersion: string;
}

/**
 * The preference-backed state the panel needs: which sections are enabled, the
 * header label, and whether the panel is collapsed. Both hosts read the same
 * shared `tui-preferences.jsonc`, so this lives here rather than being wired
 * up twice.
 *
 * `onChange` lets a host that does not repaint on its own ask for a frame
 * after preferences or the collapsed flag change.
 */
export function createSidebarPreferences(onChange?: () => void): {
  prefs: () => AftTuiPrefs;
  collapsed: () => boolean;
  toggleCollapsed: () => void;
} {
  const [prefs, setPrefs] = createSignal<AftTuiPrefs>(structuredClone(DEFAULT_PREFS));
  const [collapsed, setCollapsed] = createSignal(seedCollapsedFromPrefs(DEFAULT_PREFS));

  const reload = async (): Promise<void> => {
    const root = await readTuiPreferencesFile();
    const next = resolveAftPrefs(root);
    setPrefs(next);
    setCollapsed(seedCollapsedFromPrefs(next));
    onChange?.();
  };

  void reload();
  const unwatch = watchTuiPreferences(() => {
    void reload();
  });
  onCleanup(unwatch);

  const toggleCollapsed = (): void => {
    setCollapsed((current) => {
      const next = !current;
      persistCollapsedIfEnabled(prefs(), next);
      return next;
    });
    onChange?.();
  };

  return { prefs, collapsed, toggleCollapsed };
}

/**
 * The whole sidebar body. Both hosts render this component; neither adds rows
 * of its own, so a section added or removed here shows up in both at once.
 */
export const AftSidebarPanel = (props: AftSidebarPanelProps) => {
  const s = () => props.snapshot;

  // Lazy-bridge: while AFT has no live bridge yet, the RPC server returns a
  // synthetic snapshot with `cache_role === "not_initialized"`. In that state
  // every metric is unknown by design — not "disabled" — so we hide the
  // version line and the entire Search Index / Semantic Index / Compression
  // grid until a first tool call warms the bridge. Users were reading the
  // pre-init `vunknown` + `Status: unknown` rows as broken state instead of
  // "AFT has not been used yet for this project".
  const notInitialized = () => s()?.cache_role === "not_initialized";

  // Pre-compute display values so the JSX stays readable. createMemo for
  // each derived field would be overkill — these are cheap derivations.
  const searchStatus = () => statusDisplay(s()?.search_index?.status ?? "disabled");
  const semanticStatus = () => {
    const rawStatus = s()?.semantic_index?.status ?? "disabled";
    const display = statusDisplay(rawStatus);
    return {
      ...display,
      // The error is passed through so a capability failure (a missing ONNX
      // Runtime) is named here instead of arriving as a bare status word, and
      // an unreachable backend carries its URL and reason.
      label: formatSemanticIndexLabel({
        status: rawStatus,
        stage: s()?.semantic_index?.stage,
        error: s()?.semantic_index?.error,
        reason: s()?.semantic_index?.reason,
        backend_url: s()?.semantic_index?.backend_url,
      }),
    };
  };
  const semanticRefreshing = () =>
    formatSemanticRefreshing(s()?.semantic_index?.refreshing_count ?? 0);
  const trigramBytes = () => s()?.disk?.trigram_disk_bytes ?? 0;
  const semanticBytes = () => s()?.disk?.semantic_disk_bytes ?? 0;
  const compressionRows = () => formatCompressionSidebarRows(s()?.compression);
  const statusBar = () => s()?.status_bar;

  const degradedSummary = () => {
    const snap = s();
    if (!snap?.degraded) return null;
    const reasons = snap.degraded_reasons ?? [];
    if (reasons.length === 0) return null;
    return reasons.map(degradedReasonLabel).join("; ");
  };

  // Worktree borrow is a shared-index arrangement, not a degraded_reasons
  // entry. Keep this muted and separate from the DEGRADED badge above.
  const worktreeNote = () => worktreeCacheRoleNote(s()?.cache_role);

  return (
    <box
      width="100%"
      flexDirection="column"
      border={SINGLE_BORDER}
      borderColor={props.palette.border}
      paddingTop={1}
      paddingBottom={1}
      paddingLeft={1}
      paddingRight={1}
    >
      {/* Header: triangle toggle + AFT badge + binary version + degraded badge.
          Clicking the header row collapses/expands the panel (mirrors OpenCode's
          native MCP sidebar section). Only interactive once initialized — the
          lazy-bridge placeholder has nothing to collapse. */}
      <box
        flexDirection="row"
        justifyContent="space-between"
        alignItems="center"
        onMouseDown={() => {
          if (notInitialized()) return;
          props.onToggleCollapsed();
        }}
      >
        <box flexDirection="row" alignItems="center">
          {/* Triangle lives inside the accent badge so the toggle reads as one
              unit: "▶ AFT" / "▼ AFT". Hidden pre-init (nothing to collapse). */}
          <box paddingLeft={1} paddingRight={1} backgroundColor={props.palette.accent}>
            <text fg={badgeForeground(props.palette.accent, props.palette.background)}>
              <b>
                {notInitialized() ? "" : props.collapsed ? "▶ " : "▼ "}
                {props.prefs.header.label}
              </b>
            </text>
          </box>
          {s()?.degraded && (
            <box
              paddingLeft={1}
              paddingRight={1}
              marginLeft={1}
              backgroundColor={props.palette.warning}
            >
              <text fg={badgeForeground(props.palette.warning, props.palette.background)}>
                <b>DEGRADED</b>
              </text>
            </box>
          )}
        </box>
        {!notInitialized() && props.prefs.header.showVersion && (
          <text fg={props.palette.textMuted}>v{s()?.version ?? props.pluginVersion}</text>
        )}
      </box>

      {/* Degraded reason — explains why heavy tools (aft_search, aft_callgraph)
          are disabled. Surface this prominently so users know to open a real
          project subdirectory if they want full features. */}
      {s()?.degraded && degradedSummary() && (
        <box marginTop={1} width="100%">
          <text fg={props.palette.warning}>⚠ {degradedSummary()}</text>
        </box>
      )}

      {!notInitialized() && worktreeNote() && (
        <box marginTop={1} width="100%">
          <text fg={props.palette.textMuted}>{worktreeNote()}</text>
        </box>
      )}

      {/* Lazy-bridge placeholder. AFT skips spawning the `aft` binary at
          plugin init to keep memory/CPU low on OpenCode Desktop sessions
          that have many projects pinned in the sidebar. The RPC server
          returns a synthetic `cache_role === "not_initialized"` snapshot
          until the first tool call routes through `callBridge()` and warms
          the bridge. Show the explanatory message instead of empty status
          rows so users understand why metrics are blank. */}
      {notInitialized() && (
        <box marginTop={1} width="100%">
          <text fg={props.palette.textMuted}>{s()!.message || NOT_STARTED_STATUS_TEXT}</text>
        </box>
      )}

      {/* Collapsed view — condensed status dots + compact compression. Shown
          only when initialized AND collapsed. Three rows mirroring the section
          order of the expanded grid. */}
      {!notInitialized() && props.collapsed && (
        <box width="100%" flexDirection="column">
          {props.prefs.sections.searchIndex && (
            <CollapsedRow palette={props.palette} label="Search Index">
              <text fg={toneColor(props.palette, searchStatus().tone)}>●</text>
            </CollapsedRow>
          )}
          {props.prefs.sections.semanticIndex && (
            <CollapsedRow palette={props.palette} label="Semantic Index">
              <text fg={toneColor(props.palette, semanticStatus().tone)}>●</text>
            </CollapsedRow>
          )}
          {props.prefs.sections.codeHealth && collapsedHealthLights(statusBar()) && (
            <CollapsedRow palette={props.palette} label="Code Health">
              <box flexDirection="row" gap={1}>
                <text
                  fg={toneColor(props.palette, collapsedHealthLights(statusBar())!.diagnostics)}
                >
                  ●
                </text>
                <text fg={toneColor(props.palette, collapsedHealthLights(statusBar())!.code)}>
                  ●
                </text>
                <text fg={toneColor(props.palette, collapsedHealthLights(statusBar())!.todos)}>
                  ●
                </text>
              </box>
            </CollapsedRow>
          )}
          {props.prefs.sections.compression && collapsedCompressionValue(s()?.compression) && (
            <CollapsedRow palette={props.palette} label="Compression">
              <text fg={props.palette.textMuted}>
                <b>{collapsedCompressionValue(s()?.compression)}</b>
              </text>
            </CollapsedRow>
          )}
        </box>
      )}

      {/* Search index */}
      {!notInitialized() && !props.collapsed && (
        <>
          {props.prefs.sections.searchIndex && (
            <>
              <SectionHeader palette={props.palette} title="Search Index" />
              <StatRow
                palette={props.palette}
                label="Status"
                value={searchStatus().label}
                tone={searchStatus().tone}
              />
              {(s()?.search_index?.files ?? null) != null && (
                <StatRow
                  palette={props.palette}
                  label="Files"
                  value={formatCount(s()!.search_index.files)}
                  tone="muted"
                />
              )}
              <StatRow
                palette={props.palette}
                label="Disk"
                value={formatBytes(trigramBytes())}
                tone="muted"
              />
            </>
          )}

          {props.prefs.sections.semanticIndex && (
            <>
              <SectionHeader palette={props.palette} title="Semantic Index" />
              <StatRow
                palette={props.palette}
                label="Status"
                value={semanticStatus().label}
                tone={semanticStatus().tone}
              />
              {semanticRefreshing() && (
                <box width="100%">
                  <text fg={props.palette.textMuted}>{semanticRefreshing()}</text>
                </box>
              )}
              {/* When loading, magic-context-style progress hint helps users see
          background work is making progress instead of stuck. */}
              {s()?.semantic_index?.status === "loading" &&
                s()?.semantic_index?.entries_total != null &&
                s()!.semantic_index.entries_total! > 0 && (
                  <StatRow
                    palette={props.palette}
                    label="Progress"
                    value={`${formatCount(s()!.semantic_index.entries_done)} / ${formatCount(
                      s()!.semantic_index.entries_total,
                    )}`}
                    tone="warn"
                  />
                )}
              {(s()?.semantic_index?.entries ?? null) != null && (
                <StatRow
                  palette={props.palette}
                  label="Entries"
                  value={formatCount(s()!.semantic_index.entries)}
                  tone="muted"
                />
              )}
              <StatRow
                palette={props.palette}
                label="Disk"
                value={formatBytes(semanticBytes())}
                tone="muted"
              />
            </>
          )}

          {/* Human health values are optional. A category stays absent until the
          server proves it, rather than appearing as a clean zero. */}
          {props.prefs.sections.codeHealth && statusBar() && (
            <>
              <SectionHeader
                palette={props.palette}
                title={statusBar()!.tier2_stale ? "Code Health ~" : "Code Health"}
              />
              {statusBar()!.errors !== undefined && (
                <StatRow
                  palette={props.palette}
                  label="Errors"
                  value={formatCount(statusBar()!.errors)}
                  tone={statusBar()!.errors! > 0 ? "err" : "muted"}
                />
              )}
              {statusBar()!.warnings !== undefined && (
                <StatRow
                  palette={props.palette}
                  label="Warnings"
                  value={formatCount(statusBar()!.warnings)}
                  tone={statusBar()!.warnings! > 0 ? "warn" : "muted"}
                />
              )}
              {statusBar()!.dead_code !== undefined && (
                <StatRow
                  palette={props.palette}
                  label="Dead Code"
                  value={formatCount(statusBar()!.dead_code)}
                  tone="muted"
                />
              )}
              {statusBar()!.unused_exports !== undefined && (
                <StatRow
                  palette={props.palette}
                  label="Unused Exports"
                  value={formatCount(statusBar()!.unused_exports)}
                  tone="muted"
                />
              )}
              {statusBar()!.duplicates !== undefined && (
                <StatRow
                  palette={props.palette}
                  label="Duplicates"
                  value={formatCount(statusBar()!.duplicates)}
                  tone="muted"
                />
              )}
              {statusBar()!.todos !== undefined && (
                <StatRow
                  palette={props.palette}
                  label="TODOs"
                  value={formatCount(statusBar()!.todos)}
                  tone="muted"
                />
              )}
            </>
          )}

          {/* Compression aggregates. Tabular layout matching Search/Semantic
          Index above: each scope ("Session", "Project") renders as a
          subheader followed by two StatRows (Tokens Saved, Compression
          Ratio). Keeps numbers right-aligned in the value column instead
          of jamming them after the label on the same line. */}
          {props.prefs.sections.compression && compressionRows().length > 0 && (
            <>
              <SectionHeader palette={props.palette} title="Compression" />
              {compressionRows().map((row) =>
                row.kind === "scope" ? (
                  <box width="100%">
                    <text fg={props.palette.text}>{row.label}</text>
                  </box>
                ) : (
                  <StatRow
                    palette={props.palette}
                    label={row.label}
                    value={row.value}
                    tone="muted"
                  />
                ),
              )}
            </>
          )}

          {/* Surface failures clearly so users know to act (install ONNX,
          fix config, etc.) rather than silently leaving the panel "off". */}
          {s()?.semantic_index?.status === "failed" && s()?.semantic_index?.error && (
            <box marginTop={1} width="100%">
              <text fg={props.palette.error}>⚠ {s()!.semantic_index.error}</text>
            </box>
          )}
        </>
      )}
    </box>
  );
};
