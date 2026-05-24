/** @jsxImportSource @opentui/solid */
// @ts-nocheck

// AFT sidebar slot — mirrors opencode-magic-context's sidebar pattern.
// Header with "AFT" badge + version, then live status of search and semantic
// indexes plus their on-disk size. Refreshes on session change and on
// session.updated/message.updated events with a small debounce, same as
// magic-context, so the panel stays current without polling.

import type { TuiPluginApi, TuiSlotPlugin, TuiThemeCurrent } from "@opencode-ai/plugin/tui";
import { createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";

import { AftRpcClient } from "../shared/rpc-client";
import { type AftStatusSnapshot, coerceAftStatus, type StatusCompression } from "../shared/status";

const SINGLE_BORDER = { type: "single" } as any;
const REFRESH_DEBOUNCE_MS = 200;
// The sidebar polls the bridge as a backstop because not every state change
// (e.g. semantic index transitioning from "loading" → "ready" mid-session)
// emits a session/message event. 1.5s matches the /aft-status dialog cadence.
const POLL_INTERVAL_MS = 1500;

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

// One RPC client per project directory — same pattern as the /aft-status
// dialog handler in tui/index.tsx. Sharing the map avoids opening a second
// connection just for the sidebar.
const sidebarClients = new Map<string, AftRpcClient>();
function getClient(directory: string): AftRpcClient {
  let client = sidebarClients.get(directory);
  if (client) return client;
  const home = process.env.HOME || process.env.USERPROFILE || "";
  const dataHome = process.env.XDG_DATA_HOME || `${home}/.local/share`;
  // v0.27 moved AFT storage to the CortexKit root. Must match the server
  // plugin's `resolveCortexKitStorageRoot()` or the sidebar will poll a
  // stale legacy port file and never connect to the live RPC server.
  const storageDir = `${dataHome}/cortexkit/aft`;
  client = new AftRpcClient(storageDir, directory);
  sidebarClients.set(directory, client);
  return client;
}

const SidebarContent = (props: {
  api: TuiPluginApi;
  sessionID: () => string;
  theme: TuiThemeCurrent;
  pluginVersion: string;
}) => {
  const [status, setStatus] = createSignal<AftStatusSnapshot | null>(null);
  // Once a request is in flight, suppress any overlapping refresh so we
  // don't open a thundering herd of RPCs on rapid event bursts.
  let inflight = false;
  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  let pollTimer: ReturnType<typeof setInterval> | undefined;

  const refresh = async () => {
    const sid = props.sessionID();
    if (!sid) return;
    if (inflight) return;
    const directory = props.api.state.path.directory ?? "";
    if (!directory) return;

    inflight = true;
    try {
      const client = getClient(directory);
      const response = await client.call("status", { sessionID: sid });
      if (response && (response as Record<string, unknown>).success !== false) {
        const snapshot = coerceAftStatus(response as Record<string, unknown>);
        setStatus(snapshot);
        try {
          props.api.renderer.requestRender();
        } catch {
          // renderer may not be available during teardown; safe to ignore
        }
      }
    } catch {
      // RPC server may not be ready yet, or the bridge may be respawning
      // after a binary swap — leave the previous snapshot visible rather
      // than blanking the sidebar.
    } finally {
      inflight = false;
    }
  };

  const scheduleRefresh = () => {
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      debounceTimer = undefined;
      void refresh();
    }, REFRESH_DEBOUNCE_MS);
  };

  onCleanup(() => {
    if (debounceTimer) clearTimeout(debounceTimer);
    if (pollTimer) clearInterval(pollTimer);
  });

  // Refresh on session id change + initial load
  createEffect(
    on(props.sessionID, () => {
      void refresh();
    }),
  );

  // Wire live updates: session/message events are cheap signals that
  // *something* AFT-relevant probably changed (formatted edit, lsp activity,
  // index pre-warm completion). The status RPC is debounced so we don't
  // recompute disk usage on every keystroke.
  createEffect(
    on(
      props.sessionID,
      (sessionID) => {
        if (!sessionID) return;
        const unsubs = [
          props.api.event.on("message.updated", (event) => {
            if (event.properties?.info?.sessionID !== sessionID) return;
            scheduleRefresh();
          }),
          props.api.event.on("session.updated", (event) => {
            if (event.properties?.info?.id !== sessionID) return;
            scheduleRefresh();
          }),
        ];
        // Background poller for state that doesn't emit session events
        // (semantic index `loading` → `ready`, disk size growth during
        // a background indexer rebuild). Self-cancelling on cleanup.
        if (!pollTimer) {
          pollTimer = setInterval(() => {
            scheduleRefresh();
          }, POLL_INTERVAL_MS);
        }
        onCleanup(() => {
          for (const unsub of unsubs) {
            try {
              unsub();
            } catch {
              // best effort
            }
          }
          if (pollTimer) {
            clearInterval(pollTimer);
            pollTimer = undefined;
          }
        });
      },
      { defer: false },
    ),
  );

  const s = createMemo(() => status());

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
  const semanticStatus = () => statusDisplay(s()?.semantic_index?.status ?? "disabled");
  const trigramBytes = () => s()?.disk?.trigram_disk_bytes ?? 0;
  const semanticBytes = () => s()?.disk?.semantic_disk_bytes ?? 0;
  const compressionRows = () => formatCompressionSidebarRows(s()?.compression);

  // Degraded-mode reason → human-readable hint. Distinct strings per reason
  // because the UX direction is different: "home_root" tells the user to
  // open a real project subdirectory, "search_too_many_files" tells them the
  // tree is too big for full indexing.
  const degradedReasonLabel = (reason: string): string => {
    if (reason === "home_root") {
      return "project root is your home directory";
    }
    if (reason.startsWith("search_too_many_files:")) {
      const threshold = reason.split(":")[1] ?? "20000";
      return `project exceeds ${threshold} files`;
    }
    return reason; // unknown reason — surface verbatim so users can grep logs
  };
  const degradedSummary = () => {
    const snap = s();
    if (!snap?.degraded) return null;
    const reasons = snap.degraded_reasons ?? [];
    if (reasons.length === 0) return null;
    return reasons.map(degradedReasonLabel).join("; ");
  };

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
      {/* Header: AFT badge + binary version + degraded badge (when active) */}
      <box flexDirection="row" justifyContent="space-between" alignItems="center">
        <box flexDirection="row" alignItems="center">
          <box paddingLeft={1} paddingRight={1} backgroundColor={props.theme.accent}>
            <text fg={props.theme.background}>
              <b>AFT</b>
            </text>
          </box>
          {s()?.degraded && (
            <box
              paddingLeft={1}
              paddingRight={1}
              marginLeft={1}
              backgroundColor={props.theme.warning}
            >
              <text fg={props.theme.background}>
                <b>DEGRADED</b>
              </text>
            </box>
          )}
        </box>
        {!notInitialized() && (
          <text fg={props.theme.textMuted}>v{s()?.version ?? props.pluginVersion}</text>
        )}
      </box>

      {/* Degraded reason — explains why heavy tools (aft_search, aft_navigate)
          are disabled. Surface this prominently so users know to open a real
          project subdirectory if they want full features. */}
      {s()?.degraded && degradedSummary() && (
        <box marginTop={1} width="100%">
          <text fg={props.theme.warning}>⚠ {degradedSummary()}</text>
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

      {/* Search index */}
      {!notInitialized() && (
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

          {/* Semantic index */}
          <SectionHeader theme={props.theme} title="Semantic Index" />
          <StatRow
            theme={props.theme}
            label="Status"
            value={semanticStatus().label}
            tone={semanticStatus().tone}
          />
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

          {/* Compression aggregates. Tabular layout matching Search/Semantic
          Index above: each scope ("Session", "Project") renders as a
          subheader followed by two StatRows (Tokens Saved, Compression
          Ratio). Keeps numbers right-aligned in the value column instead
          of jamming them after the label on the same line. */}
          {compressionRows().length > 0 && (
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

export function createAftSidebarSlot(api: TuiPluginApi, pluginVersion: string): TuiSlotPlugin {
  return {
    // 150 matches magic-context's order — chosen so AFT renders below
    // higher-priority panels but above default plugin slots. If both
    // plugins are loaded together, magic-context will appear first.
    order: 160,
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
