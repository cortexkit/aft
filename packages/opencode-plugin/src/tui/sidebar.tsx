/** @jsxImportSource @opentui/solid */
// @ts-nocheck

// AFT sidebar slot. Header with "AFT" badge + version, then live status of search and semantic
// indexes plus their on-disk size. Refreshes on mount/session change and on
// server-pushed status invalidations with a small debounce, so the panel stays
// current without polling.

import { canonicalizeProjectRoot } from "@cortexkit/aft-bridge";
import type { TuiPluginApi, TuiSlotPlugin, TuiThemeCurrent } from "@opencode/plugin/tui";
import { createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import { AftRpcClient } from "../shared/rpc-client";
import {
  type AftStatusSnapshot,
  coerceAftStatus,
  formatSemanticIndexStatus,
  formatSemanticRefreshing,
  type StatusBar,
  type StatusCompression,
  worktreeCacheRoleNote,
} from "../shared/status";
import { resolveCortexKitStorageRoot } from "../shared/storage-paths";
import { badgeTextColor } from "./badge-contrast";
import {
  createDebouncedStatusRefresh,
  refreshAftTuiSocketScope,
  subscribeStatusInvalidations,
} from "./notification-socket";
import {
  type AftTuiPrefs,
  computeEffectiveOrder,
  DEFAULT_PREFS,
  DEFAULT_SLOT_ORDER,
  PLUGIN_KEY,
  persistCollapsedIfEnabled,
  readTuiPreferencesFile,
  resolveAftPrefs,
  seedCollapsedFromPrefs,
  watchTuiPreferences,
} from "./preferences";

const SINGLE_BORDER = { type: "single" } as any;
const REFRESH_DEBOUNCE_MS = 200;

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

// Map index status → (label, theme color name). The label is what we want
// the user to see; the color encodes severity so the eye lands on warnings.
function statusDisplay(status: string): { label: string; tone: "ok" | "warn" | "err" | "muted" } {
  switch (status) {
    case "ready":
      return { label: "ready", tone: "ok" };
    case "loading":
    case "building":
      return { label: status, tone: "warn" };
    case "failed":
    case "error":
      return { label: status, tone: "err" };
    case "disabled":
      return { label: "disabled", tone: "muted" };
    default:
      return { label: status || "unknown", tone: "muted" };
  }
}

const StatRow = (props: {
  theme: TuiThemeCurrent;
  label: string;
  value: string;
  tone?: "ok" | "warn" | "err" | "muted" | "accent";
}) => {
  const fg = createMemo(() => {
    switch (props.tone) {
      case "ok":
        return props.theme.success ?? props.theme.accent;
      case "warn":
        return props.theme.warning;
      case "err":
        return props.theme.error;
      case "muted":
        return props.theme.textMuted;
      case "accent":
        return props.theme.accent;
      default:
        return props.theme.text;
    }
  });

  return (
    <box width="100%" flexDirection="row" justifyContent="space-between">
      <text fg={props.theme.textMuted}>{props.label}</text>
      <text fg={fg()}>
        <b>{props.value}</b>
      </text>
    </box>
  );
};

const SectionHeader = (props: { theme: TuiThemeCurrent; title: string; marginTop?: number }) => (
  <box width="100%" marginTop={props.marginTop ?? 1}>
    <text fg={props.theme.text}>
      <b>{props.title}</b>
    </text>
  </box>
);

// Map a status tone to a theme color — used for the collapsed-view status dots.
function toneColor(theme: TuiThemeCurrent, tone: "ok" | "warn" | "err" | "muted"): string {
  switch (tone) {
    case "ok":
      return theme.success ?? theme.accent;
    case "warn":
      return theme.warning;
    case "err":
      return theme.error;
    default:
      return theme.textMuted;
  }
}

// Collapsed-view row: label on the left, a status dot (or compact value) on the
// right. Mirrors the expanded StatRow layout so the columns line up.
const CollapsedRow = (props: { theme: TuiThemeCurrent; label: string; children: JSX.Element }) => (
  <box width="100%" flexDirection="row" justifyContent="space-between">
    <text fg={props.theme.textMuted}>{props.label}</text>
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

// Keep the TUI on the bridge's shared resolver so its root matches the
// configure payload used by the plugin and binary.
export function resolveTuiStorageDir(): string {
  return resolveCortexKitStorageRoot();
}

// One RPC client per project directory — same pattern as the /aft-status
// dialog handler in tui/index.tsx. Sharing the map avoids opening a second
// connection just for the sidebar.
const sidebarClients = new Map<string, AftRpcClient>();
function getClient(directory: string): AftRpcClient {
  let client = sidebarClients.get(directory);
  if (client) return client;
  client = new AftRpcClient(resolveTuiStorageDir(), directory);
  sidebarClients.set(directory, client);
  return client;
}

export type ScopedSidebarStatus = {
  directory: string;
  sessionID: string;
  snapshot: AftStatusSnapshot;
};

export function scopedSidebarSnapshot(
  scoped: ScopedSidebarStatus | null,
  directory: string,
  sessionID: string,
): AftStatusSnapshot | null {
  if (!scoped) return null;
  if (scoped.directory !== directory || scoped.sessionID !== sessionID) return null;
  return scoped.snapshot;
}

/**
 * Stale-while-revalidate guard. A transient `not_initialized` snapshot (bridge
 * mid-respawn after a binary swap, or a momentary session-dir key miss) arrives
 * over RPC as `success: true`, so a naive `setStatus` would overwrite a good
 * snapshot and collapse the panel to the lazy-bridge placeholder — the blank
 * flicker that recovers on the next refresh. Suppress the downgrade only when we
 * already hold initialized data for the same context; never blocks the first
 * real snapshot, and a genuine context switch clears separately.
 */
export function shouldSuppressUninitializedDowngrade(
  incomingCacheRole: string | undefined,
  haveInitializedForContext: boolean,
): boolean {
  return incomingCacheRole === "not_initialized" && haveInitializedForContext;
}

/**
 * Cross-project contamination belt. The RPC layer can hand back a snapshot
 * describing a DIFFERENT project than the one this sidebar asked about — a
 * multi-project host (Desktop / `opencode serve`) whose status handler
 * resolved another project's warm bridge, including long-lived processes
 * still running pre-fix plugin code. Rendering it shows another repo's
 * indexes/health in this window.
 *
 * A mismatched project_root is acceptable ONLY when the serving handler says
 * it resolved that directory DELIBERATELY: new servers attach
 * `served_directory` (their own cwd, or the SDK-verified `opencode -s` resume
 * directory) to every status response. That marker is handler-attached
 * provenance — it cannot be faked by snapshot contents. We explicitly do NOT
 * use `snapshot.session.id` here: Rust echoes the REQUESTED session id into
 * the snapshot, so it matches even when the data came from another project's
 * bridge (the hole that let contamination through this belt's first version).
 *
 * Rules:
 *  - placeholder/synthetic snapshots (no project_root) → accept (not data)
 *  - project_root (or canonical_root) matches the sidebar directory → accept
 *  - mismatched root AND served_directory matches a snapshot root → accept
 *    (deliberate, SDK-verified resume serve from a new server)
 *  - otherwise → reject (stray; includes everything old servers cross-serve)
 */
export function isSnapshotForContext(
  snapshot: AftStatusSnapshot,
  directory: string,
  servedDirectory: string | undefined,
): boolean {
  // Canonicalize both sides through the SAME canonicalizer the bridge routes
  // by, so a symlinked / `/var`-vs-`/private/var` / trailing-slash spelling of
  // the sidebar directory still matches Rust's canonical_root. A raw stripSlash
  // compare (the old behavior) rejected legitimate snapshots whenever the TUI
  // directory and Rust's root were different spellings of the same location,
  // leaving the sidebar blank on aliased roots.
  const canon = (p: string) => canonicalizeProjectRoot(p);
  const roots = [snapshot.project_root, snapshot.canonical_root].filter(
    (r): r is string => typeof r === "string" && r.length > 0,
  );
  if (roots.length === 0) return true; // placeholder / synthetic
  const dir = canon(directory);
  if (roots.some((r) => canon(r) === dir)) return true;
  if (typeof servedDirectory === "string" && servedDirectory.length > 0) {
    const served = canon(servedDirectory);
    return roots.some((r) => canon(r) === served);
  }
  return false;
}

const SidebarContent = (props: {
  api: TuiPluginApi;
  sessionID: () => string;
  theme: TuiThemeCurrent;
  pluginVersion: string;
}) => {
  const [status, setStatus] = createSignal<ScopedSidebarStatus | null>(null);
  const [prefs, setPrefs] = createSignal<AftTuiPrefs>(structuredClone(DEFAULT_PREFS));
  const [collapsed, setCollapsed] = createSignal(seedCollapsedFromPrefs(DEFAULT_PREFS));
  let inflight: {
    controller: AbortController;
    generation: number;
    directory: string;
    sessionID: string;
  } | null = null;
  let generation = 0;

  const currentDirectory = () => props.api.state.path.directory ?? "";
  const requestRender = () => {
    try {
      props.api.renderer.requestRender();
    } catch {
      // renderer may not be available during teardown; safe to ignore
    }
  };
  const abortInflight = () => {
    if (!inflight) return;
    inflight.controller.abort();
    inflight = null;
  };
  const clearStatusForContext = (directory: string, sessionID: string) => {
    const current = status();
    if (!current) return;
    if (current.directory === directory && current.sessionID === sessionID) return;
    setStatus(null);
    requestRender();
  };

  const refresh = async () => {
    const sid = props.sessionID();
    const directory = currentDirectory();
    if (!sid || !directory) {
      generation++;
      abortInflight();
      if (status()) {
        setStatus(null);
        requestRender();
      }
      return;
    }

    clearStatusForContext(directory, sid);

    if (inflight) {
      if (inflight.directory === directory && inflight.sessionID === sid) return;
      generation++;
      abortInflight();
    }

    const requestGeneration = ++generation;
    const controller = new AbortController();
    inflight = { controller, generation: requestGeneration, directory, sessionID: sid };

    try {
      const client = getClient(directory);
      const response = await client.call(
        "status",
        { sessionID: sid },
        {
          signal: controller.signal,
          // With several RPC servers alive for this project hash, a stray
          // warm response (another project's bridge) must not beat the right
          // server or the placeholder — skip it at the port-scan level.
          accept: (result) => {
            const rec = result as Record<string, unknown>;
            if (rec?.success === false) return true; // errors handled below
            return isSnapshotForContext(
              coerceAftStatus(rec),
              directory,
              rec?.served_directory as string | undefined,
            );
          },
        },
      );
      if (controller.signal.aborted || requestGeneration !== generation) return;
      if (currentDirectory() !== directory || props.sessionID() !== sid) return;
      if (response && (response as Record<string, unknown>).success !== false) {
        const snapshot = coerceAftStatus(response as Record<string, unknown>);
        // Belt: never render a snapshot describing another project (see
        // isSnapshotForContext). Keep whatever we currently show instead.
        const servedDirectory = (response as Record<string, unknown>).served_directory as
          | string
          | undefined;
        if (!isSnapshotForContext(snapshot, directory, servedDirectory)) return;
        // Stale-while-revalidate: keep the last-good snapshot instead of
        // flickering to the lazy-bridge placeholder on a transient
        // not_initialized. See shouldSuppressUninitializedDowngrade.
        const current = status();
        const haveGoodForContext =
          current !== null &&
          current.directory === directory &&
          current.sessionID === sid &&
          current.snapshot.cache_role !== "not_initialized";
        if (shouldSuppressUninitializedDowngrade(snapshot.cache_role, haveGoodForContext)) return;
        // Equality gate: a pushed invalidation can still produce the same
        // snapshot (for example, a session-scoped status frame that does not
        // affect this sidebar's visible fields). Minting a new status object
        // would run SolidJS reactivity and schedule a host frame for no visible
        // change. Skip the update when the freshly-fetched snapshot is
        // byte-identical to what we already show for this exact context.
        // JSON.stringify is sound here because the snapshot is a plain object
        // coerced from the status RPC's JSON.
        if (
          current !== null &&
          current.directory === directory &&
          current.sessionID === sid &&
          JSON.stringify(current.snapshot) === JSON.stringify(snapshot)
        ) {
          return;
        }
        setStatus({ directory, sessionID: sid, snapshot });
        requestRender();
      }
    } catch {
      if (controller.signal.aborted || requestGeneration !== generation) return;
      // RPC server may not be ready yet, or the bridge may be respawning
      // after a binary swap. Keep the previous snapshot only when it belongs
      // to the current project/session; mismatched snapshots were cleared above.
    } finally {
      if (inflight?.generation === requestGeneration) inflight = null;
    }
  };

  const statusDebouncer = createDebouncedStatusRefresh(refresh, REFRESH_DEBOUNCE_MS);
  const scheduleRefresh = () => statusDebouncer.schedule();

  const reloadPrefs = async () => {
    const root = await readTuiPreferencesFile();
    const next = resolveAftPrefs(root);
    setPrefs(next);
    setCollapsed(seedCollapsedFromPrefs(next));
    requestRender();
  };

  void reloadPrefs();
  const unwatchPrefs = watchTuiPreferences(() => {
    void reloadPrefs();
  });

  onCleanup(() => {
    unwatchPrefs();
    generation++;
    abortInflight();
    statusDebouncer.dispose();
  });

  // Refresh on session id change + initial load
  createEffect(
    on(props.sessionID, () => {
      refreshAftTuiSocketScope();
      void refresh();
    }),
  );

  // Wire live updates: the server pushes a lightweight invalidation whenever
  // the bridge reports a status change. The sidebar coalesces bursts into one
  // trailing status fetch and stays completely idle when no backend state changes.
  createEffect(
    on(
      props.sessionID,
      (sessionID) => {
        if (!sessionID) return;
        const unsubscribe = subscribeStatusInvalidations((event) => {
          if (event.sessionId && event.sessionId !== props.sessionID()) return;
          scheduleRefresh();
        });
        onCleanup(() => {
          unsubscribe();
          generation++;
          abortInflight();
        });
      },
      { defer: false },
    ),
  );

  const s = () => scopedSidebarSnapshot(status(), currentDirectory(), props.sessionID());

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
      label: formatSemanticIndexStatus(rawStatus, s()?.semantic_index?.stage),
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
      borderColor={props.theme.borderActive}
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
          setCollapsed((x) => {
            const next = !x;
            persistCollapsedIfEnabled(prefs(), next);
            return next;
          });
        }}
      >
        <box flexDirection="row" alignItems="center">
          {/* Triangle lives inside the accent badge so the toggle reads as one
              unit: "▶ AFT" / "▼ AFT". Hidden pre-init (nothing to collapse). */}
          <box paddingLeft={1} paddingRight={1} backgroundColor={props.theme.accent}>
            <text fg={badgeTextColor(props.theme.accent, props.theme.background)}>
              <b>
                {notInitialized() ? "" : collapsed() ? "▶ " : "▼ "}
                {prefs().header.label}
              </b>
            </text>
          </box>
          {s()?.degraded && (
            <box
              paddingLeft={1}
              paddingRight={1}
              marginLeft={1}
              backgroundColor={props.theme.warning}
            >
              <text fg={badgeTextColor(props.theme.warning, props.theme.background)}>
                <b>DEGRADED</b>
              </text>
            </box>
          )}
        </box>
        {!notInitialized() && prefs().header.showVersion && (
          <text fg={props.theme.textMuted}>v{s()?.version ?? props.pluginVersion}</text>
        )}
      </box>

      {/* Degraded reason — explains why heavy tools (aft_search, aft_callgraph)
          are disabled. Surface this prominently so users know to open a real
          project subdirectory if they want full features. */}
      {s()?.degraded && degradedSummary() && (
        <box marginTop={1} width="100%">
          <text fg={props.theme.warning}>⚠ {degradedSummary()}</text>
        </box>
      )}

      {!notInitialized() && worktreeNote() && (
        <box marginTop={1} width="100%">
          <text fg={props.theme.textMuted}>{worktreeNote()}</text>
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
          <text fg={props.theme.textMuted}>
            {s()!.message ||
              "AFT bridge is now spawned lazily, information here will be populated after first tool call."}
          </text>
        </box>
      )}

      {/* Collapsed view — condensed status dots + compact compression. Shown
          only when initialized AND collapsed. Three rows mirroring the section
          order of the expanded grid. */}
      {!notInitialized() && collapsed() && (
        <box width="100%" flexDirection="column">
          {prefs().sections.searchIndex && (
            <CollapsedRow theme={props.theme} label="Search Index">
              <text fg={toneColor(props.theme, searchStatus().tone)}>●</text>
            </CollapsedRow>
          )}
          {prefs().sections.semanticIndex && (
            <CollapsedRow theme={props.theme} label="Semantic Index">
              <text fg={toneColor(props.theme, semanticStatus().tone)}>●</text>
            </CollapsedRow>
          )}
          {prefs().sections.codeHealth && collapsedHealthLights(statusBar()) && (
            <CollapsedRow theme={props.theme} label="Code Health">
              <box flexDirection="row" gap={1}>
                <text fg={toneColor(props.theme, collapsedHealthLights(statusBar())!.diagnostics)}>
                  ●
                </text>
                <text fg={toneColor(props.theme, collapsedHealthLights(statusBar())!.code)}>●</text>
                <text fg={toneColor(props.theme, collapsedHealthLights(statusBar())!.todos)}>
                  ●
                </text>
              </box>
            </CollapsedRow>
          )}
          {prefs().sections.compression && collapsedCompressionValue(s()?.compression) && (
            <CollapsedRow theme={props.theme} label="Compression">
              <text fg={props.theme.textMuted}>
                <b>{collapsedCompressionValue(s()?.compression)}</b>
              </text>
            </CollapsedRow>
          )}
        </box>
      )}

      {/* Search index */}
      {!notInitialized() && !collapsed() && (
        <>
          {prefs().sections.searchIndex && (
            <>
              <SectionHeader theme={props.theme} title="Search Index" />
              <StatRow
                theme={props.theme}
                label="Status"
                value={searchStatus().label}
                tone={searchStatus().tone}
              />
              {(s()?.search_index?.files ?? null) != null && (
                <StatRow
                  theme={props.theme}
                  label="Files"
                  value={formatCount(s()!.search_index.files)}
                  tone="muted"
                />
              )}
              <StatRow
                theme={props.theme}
                label="Disk"
                value={formatBytes(trigramBytes())}
                tone="muted"
              />
            </>
          )}

          {prefs().sections.semanticIndex && (
            <>
              <SectionHeader theme={props.theme} title="Semantic Index" />
              <StatRow
                theme={props.theme}
                label="Status"
                value={semanticStatus().label}
                tone={semanticStatus().tone}
              />
              {semanticRefreshing() && (
                <box width="100%">
                  <text fg={props.theme.textMuted}>{semanticRefreshing()}</text>
                </box>
              )}
              {/* When loading, magic-context-style progress hint helps users see
          background work is making progress instead of stuck. */}
              {s()?.semantic_index?.status === "loading" &&
                s()?.semantic_index?.entries_total != null &&
                s()!.semantic_index.entries_total! > 0 && (
                  <StatRow
                    theme={props.theme}
                    label="Progress"
                    value={`${formatCount(s()!.semantic_index.entries_done)} / ${formatCount(
                      s()!.semantic_index.entries_total,
                    )}`}
                    tone="warn"
                  />
                )}
              {(s()?.semantic_index?.entries ?? null) != null && (
                <StatRow
                  theme={props.theme}
                  label="Entries"
                  value={formatCount(s()!.semantic_index.entries)}
                  tone="muted"
                />
              )}
              <StatRow
                theme={props.theme}
                label="Disk"
                value={formatBytes(semanticBytes())}
                tone="muted"
              />
            </>
          )}

          {/* Human health values are optional. A category stays absent until the
          server proves it, rather than appearing as a clean zero. */}
          {prefs().sections.codeHealth && statusBar() && (
            <>
              <SectionHeader
                theme={props.theme}
                title={statusBar()!.tier2_stale ? "Code Health ~" : "Code Health"}
              />
              {statusBar()!.errors !== undefined && (
                <StatRow
                  theme={props.theme}
                  label="Errors"
                  value={formatCount(statusBar()!.errors)}
                  tone={statusBar()!.errors! > 0 ? "err" : "muted"}
                />
              )}
              {statusBar()!.warnings !== undefined && (
                <StatRow
                  theme={props.theme}
                  label="Warnings"
                  value={formatCount(statusBar()!.warnings)}
                  tone={statusBar()!.warnings! > 0 ? "warn" : "muted"}
                />
              )}
              {statusBar()!.dead_code !== undefined && (
                <StatRow
                  theme={props.theme}
                  label="Dead Code"
                  value={formatCount(statusBar()!.dead_code)}
                  tone="muted"
                />
              )}
              {statusBar()!.unused_exports !== undefined && (
                <StatRow
                  theme={props.theme}
                  label="Unused Exports"
                  value={formatCount(statusBar()!.unused_exports)}
                  tone="muted"
                />
              )}
              {statusBar()!.duplicates !== undefined && (
                <StatRow
                  theme={props.theme}
                  label="Duplicates"
                  value={formatCount(statusBar()!.duplicates)}
                  tone="muted"
                />
              )}
              {statusBar()!.todos !== undefined && (
                <StatRow
                  theme={props.theme}
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
          {prefs().sections.compression && compressionRows().length > 0 && (
            <>
              <SectionHeader theme={props.theme} title="Compression" />
              {compressionRows().map((row) =>
                row.kind === "scope" ? (
                  <box width="100%">
                    <text fg={props.theme.text}>{row.label}</text>
                  </box>
                ) : (
                  <StatRow theme={props.theme} label={row.label} value={row.value} tone="muted" />
                ),
              )}
            </>
          )}

          {/* Surface failures clearly so users know to act (install ONNX,
          fix config, etc.) rather than silently leaving the panel "off". */}
          {s()?.semantic_index?.status === "failed" && s()?.semantic_index?.error && (
            <box marginTop={1} width="100%">
              <text fg={props.theme.error}>⚠ {s()!.semantic_index.error}</text>
            </box>
          )}
        </>
      )}
    </box>
  );
};

export async function createAftSidebarSlot(
  api: TuiPluginApi,
  pluginVersion: string,
): Promise<TuiSlotPlugin> {
  const root = await readTuiPreferencesFile();
  const order = computeEffectiveOrder(root, PLUGIN_KEY, DEFAULT_SLOT_ORDER);
  return {
    // DEFAULT_SLOT_ORDER (180) is AFT's coordinated default in the shared
    // tui-preferences ladder (anthropic-auth 160, AFT 180, magic-context 200).
    // Override via `order` or `forceToTop` in tui-preferences.jsonc.
    order,
    slots: {
      sidebar_content: (ctx, value) => {
        const theme = createMemo(() => (ctx as any).theme.current);
        return (
          <SidebarContent
            api={api}
            sessionID={() => value.session_id}
            theme={theme()}
            pluginVersion={pluginVersion}
          />
        );
      },
    },
  };
}
