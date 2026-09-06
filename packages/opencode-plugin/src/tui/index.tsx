/** @jsxImportSource @opentui/solid */
// @ts-nocheck

import type { TuiPlugin, TuiPluginApi, TuiThemeCurrent } from "@opencode-ai/plugin/tui";
import { createMemo, createSignal, onCleanup } from "solid-js";

import { version as packageVersion } from "../../package.json";
import { AftRpcClient } from "../shared/rpc-client";
import {
  type AftStatusSnapshot,
  coerceAftStatus,
  formatBytes,
  formatCacheRoleLabel,
  formatSemanticIndexStatus,
  formatSemanticRefreshing,
} from "../shared/status";
import {
  createDebouncedStatusRefresh,
  refreshAftTuiSocketScope,
  type SocketNotification,
  startAftTuiSocket,
  stopAftTuiSocket,
  subscribeStatusInvalidations,
} from "./notification-socket";
import {
  createAftSidebarSlot,
  formatCompressionSidebarRows,
  isSnapshotForContext,
  resolveTuiStorageDir,
  shouldSuppressUninitializedDowngrade,
} from "./sidebar";
import { setupV2Tui } from "./v2";

// The TUI talks to the server plugin via AftRpcClient. The client reads the
// JSON port file written by AftRpcServer ({ port, token }) and includes that
// per-server token on every RPC request; legacy integer port files are still
// tolerated for already-running older server plugins.

// RPC clients keyed by directory — one per project
const rpcClients = new Map<string, AftRpcClient>();

function getRpcClient(directory: string): AftRpcClient {
  let client = rpcClients.get(directory);
  if (client) return client;

  client = new AftRpcClient(resolveTuiStorageDir(), directory);
  rpcClients.set(directory, client);
  return client;
}

function getSessionId(api: TuiPluginApi): string | null {
  try {
    const route = api.route.current;
    if (route?.name === "session" && route.params?.sessionID) {
      return route.params.sessionID;
    }
  } catch {
    // ignore
  }
  return null;
}

// ---------------------------------------------------------------------------
// StatusDialog — themed, two-column JSX dialog. OpenCode renders DialogAlert
// text in a proportional font with no column alignment, so the status view uses
// TUI flex primitives instead of a padded monospace string. The component
// subscribes to server-pushed invalidations so it can re-render as the status
// snapshot changes, with no parent re-mount.
// ---------------------------------------------------------------------------

function formatCountShort(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${Math.round(value / 1_000)}K`;
  return String(value);
}

function statusTone(status: string): "ok" | "warn" | "err" | "muted" {
  switch (status) {
    case "ready":
      return "ok";
    case "loading":
    case "building":
      return "warn";
    case "failed":
    case "error":
      return "err";
    default:
      return "muted";
  }
}

function pickToneColor(theme: TuiThemeCurrent, tone: "ok" | "warn" | "err" | "muted"): string {
  switch (tone) {
    case "ok":
      return (theme as any).success ?? theme.accent;
    case "warn":
      return theme.warning;
    case "err":
      return theme.error;
    case "muted":
      return theme.textMuted;
  }
}

/**
 * Label/value row. Label is left-aligned and muted; value is right-aligned
 * and themed. flexDirection="row" + justifyContent="space-between" replaces
 * the monospace `padEnd(40)` hack from the previous string formatter.
 */
const R = (props: {
  theme: TuiThemeCurrent;
  label: string;
  value: string;
  tone?: "ok" | "warn" | "err" | "muted" | "accent";
}) => {
  const fg = createMemo(() => {
    if (!props.tone) return props.theme.text;
    if (props.tone === "accent") return props.theme.accent;
    return pickToneColor(props.theme, props.tone);
  });

  return (
    <box flexDirection="row" width="100%" justifyContent="space-between">
      <text fg={props.theme.textMuted}>{props.label}</text>
      <text fg={fg()}>{props.value}</text>
    </box>
  );
};

interface StatusDialogProps {
  api: TuiPluginApi;
  client: AftRpcClient;
  directory: string;
  sessionID: string;
  initial: AftStatusSnapshot | null;
  initialError: string | null;
}

/**
 * Shared accept-gate for status RPC calls: skip warm responses that describe
 * another project (cross-project contamination from multi-project hosts —
 * see isSnapshotForContext) so they can't beat the right server's response.
 */
function statusAcceptGate(directory: string): (result: unknown) => boolean {
  return (result) => {
    const rec = result as Record<string, unknown>;
    if (rec?.success === false) return true; // error envelopes handled by callers
    return isSnapshotForContext(
      coerceAftStatus(rec),
      directory,
      rec?.served_directory as string | undefined,
    );
  };
}

const StatusDialog = (props: StatusDialogProps) => {
  const theme = createMemo(() => (props.api as any).theme.current as TuiThemeCurrent);
  const t = () => theme();

  // Reactive status signal — the dialog re-renders on pushed status
  // invalidations without remounting.
  const [status, setStatus] = createSignal<AftStatusSnapshot | null>(props.initial);
  const [error, setError] = createSignal<string | null>(props.initialError);

  let refreshGeneration = 0;
  let refreshController: AbortController | null = null;
  const refreshStatus = async () => {
    if (refreshController) return;

    const controller = new AbortController();
    const requestGeneration = ++refreshGeneration;
    refreshController = controller;

    try {
      const response = await props.client.call(
        "status",
        { sessionID: props.sessionID },
        { signal: controller.signal, accept: statusAcceptGate(props.directory) },
      );
      if (controller.signal.aborted || requestGeneration !== refreshGeneration) return;
      if ((response as Record<string, unknown>).success !== false) {
        const snapshot = coerceAftStatus(response as Record<string, unknown>);
        // Stale-while-revalidate: don't downgrade a good snapshot to a transient
        // `not_initialized` (for example, while the bridge process respawns or
        // a session directory lookup briefly misses) — it arrives as success:true
        // and would blank the dialog until the next refresh.
        const current = status();
        if (
          shouldSuppressUninitializedDowngrade(
            snapshot.cache_role,
            current !== null && current.cache_role !== "not_initialized",
          )
        ) {
          return;
        }
        setStatus(snapshot);
        setError(null);
      }
    } catch {
      if (controller.signal.aborted || requestGeneration !== refreshGeneration) return;
      // transient — keep showing last good snapshot
    } finally {
      if (refreshController === controller) refreshController = null;
    }
  };

  const statusDebouncer = createDebouncedStatusRefresh(refreshStatus, 200);
  const unsubscribeStatusInvalidations = subscribeStatusInvalidations((event) => {
    if (event.sessionId && event.sessionId !== props.sessionID) return;
    statusDebouncer.schedule();
  });
  onCleanup(() => {
    unsubscribeStatusInvalidations();
    statusDebouncer.dispose();
    refreshGeneration++;
    if (refreshController) {
      refreshController.abort();
      refreshController = null;
    }
  });

  // Visual cache-role badge: main is accent. Worktree/read_only borrow the
  // shared repo index — muted, not warning, so it is not read as a failure.
  const cacheRoleTone = (role: string): "accent" | "warn" | "muted" =>
    role === "main" ? "accent" : "muted";
  // Reuse the sidebar's label/value formatter so the dialog and sidebar
  // render identical text (e.g. "Session" / "-174,489 tokens, 59% reduction").
  // The earlier `formatCompressionDialogRows` returned padded strings that
  // looked offset against neighboring sections.
  const compressionAggregateRows = () => formatCompressionSidebarRows(status()?.compression);

  return (
    <box
      flexDirection="column"
      width="100%"
      paddingLeft={2}
      paddingRight={2}
      paddingTop={1}
      paddingBottom={1}
    >
      {/* Title. Hide version while the lazy-spawn placeholder is showing — users
          read `vunknown` next to "AFT Status" as broken state instead of "AFT
          has not been used yet for this project". */}
      <box justifyContent="center" width="100%" marginBottom={1} flexDirection="row" gap={2}>
        <text fg={t().accent}>
          <b>⚡ AFT Status</b>
        </text>
        {status()?.cache_role !== "not_initialized" && (
          <text fg={t().textMuted}>v{status()?.version ?? packageVersion}</text>
        )}
      </box>

      {/* Error / not-yet-ready state */}
      {error() ? (
        <box width="100%" marginBottom={1}>
          <text fg={t().warning}>{error()}</text>
        </box>
      ) : null}

      {/* Lazy-bridge placeholder: when no bridge has spawned for this project
          yet (because the user has not made any tool call since opening
          OpenCode), the RPC server returns a synthetic snapshot with
          cache_role === "not_initialized". Show the explanatory message
          instead of an empty grid of zeros and "unknown" rows. */}
      {status()?.cache_role === "not_initialized" ? (
        <box width="100%" marginTop={1} justifyContent="center">
          <text fg={t().textMuted}>
            {status()!.message ||
              "AFT bridge is now spawned lazily, information here will be populated after first tool call."}
          </text>
        </box>
      ) : null}

      {/* Header rows — paths span full width since they can be long */}
      {status() && status()!.cache_role !== "not_initialized" ? (
        <box flexDirection="column" width="100%" marginBottom={1}>
          <R
            theme={t()}
            label="Project root"
            value={status()!.project_root ?? "(not configured)"}
          />
          <R
            theme={t()}
            label="Canonical root"
            value={status()!.canonical_root ?? "(not configured)"}
          />
          <R
            theme={t()}
            label="Cache role"
            value={formatCacheRoleLabel(status()!.cache_role)}
            tone={cacheRoleTone(status()!.cache_role)}
          />
        </box>
      ) : null}

      {/* 2-column body. Gate on cache_role too so a synthetic not_initialized
          snapshot doesn't render an empty grid of "unknown" / "—" rows
          alongside the lazy-spawn placeholder message above. */}
      {status() && status()!.cache_role !== "not_initialized" ? (
        <box flexDirection="row" width="100%" gap={4}>
          {/* Left column */}
          <box flexDirection="column" flexGrow={1} flexBasis={0}>
            <text fg={t().text}>
              <b>Search index</b>
            </text>
            <R
              theme={t()}
              label="Status"
              value={status()!.search_index.status}
              tone={statusTone(status()!.search_index.status)}
            />
            <R theme={t()} label="Files" value={formatCountShort(status()!.search_index.files)} />
            <R
              theme={t()}
              label="Trigrams"
              value={formatCountShort(status()!.search_index.trigrams)}
            />
            <R
              theme={t()}
              label="Disk"
              value={formatBytes(status()!.disk.trigram_disk_bytes)}
              tone="muted"
            />

            <box marginTop={1}>
              <text fg={t().text}>
                <b>Runtime</b>
              </text>
            </box>
            <R theme={t()} label="LSP servers" value={String(status()!.lsp_servers)} />
            <R
              theme={t()}
              label="Symbol cache (local)"
              value={formatCountShort(status()!.symbol_cache.local_entries)}
            />
            <R
              theme={t()}
              label="Symbol cache (warm)"
              value={formatCountShort(status()!.symbol_cache.warm_entries)}
              tone="muted"
            />

            <box marginTop={1}>
              <text fg={t().text}>
                <b>Features</b>
              </text>
            </box>
            <R
              theme={t()}
              label="format_on_edit"
              value={status()!.features.format_on_edit ? "on" : "off"}
              tone={status()!.features.format_on_edit ? "ok" : "muted"}
            />
            <R
              theme={t()}
              label="search_index"
              value={status()!.features.search_index ? "on" : "off"}
              tone={status()!.features.search_index ? "ok" : "muted"}
            />
            <R
              theme={t()}
              label="semantic_search"
              value={status()!.features.semantic_search ? "on" : "off"}
              tone={status()!.features.semantic_search ? "ok" : "muted"}
            />
            <R
              theme={t()}
              label="callgraph_store"
              value={status()!.features.callgraph_store ? "on" : "off"}
              tone={status()!.features.callgraph_store ? "ok" : "muted"}
            />
          </box>

          {/* Right column */}
          <box flexDirection="column" flexGrow={1} flexBasis={0}>
            <text fg={t().text}>
              <b>Semantic index</b>
            </text>
            <R
              theme={t()}
              label="Status"
              value={formatSemanticIndexStatus(
                status()!.semantic_index.status,
                status()!.semantic_index.stage,
              )}
              tone={statusTone(status()!.semantic_index.status)}
            />
            {formatSemanticRefreshing(status()!.semantic_index.refreshing_count) ? (
              <box width="100%">
                <text fg={t().textMuted}>
                  {formatSemanticRefreshing(status()!.semantic_index.refreshing_count)}
                </text>
              </box>
            ) : null}
            <R
              theme={t()}
              label="Entries"
              value={formatCountShort(status()!.semantic_index.entries)}
            />
            {status()!.semantic_index.backend ? (
              <R
                theme={t()}
                label="Backend"
                value={status()!.semantic_index.backend!}
                tone="muted"
              />
            ) : null}
            {status()!.semantic_index.model ? (
              <R theme={t()} label="Model" value={status()!.semantic_index.model!} tone="muted" />
            ) : null}
            {status()!.semantic_index.dimension != null ? (
              <R
                theme={t()}
                label="Dimension"
                value={String(status()!.semantic_index.dimension)}
                tone="muted"
              />
            ) : null}
            <R
              theme={t()}
              label="Disk"
              value={formatBytes(status()!.disk.semantic_disk_bytes)}
              tone="muted"
            />

            <box marginTop={1}>
              <text fg={t().text}>
                <b>Current session</b>
              </text>
            </box>
            <R theme={t()} label="Tracked files" value={String(status()!.session.tracked_files)} />
            <R theme={t()} label="Checkpoints" value={String(status()!.session.checkpoints)} />
            <R
              theme={t()}
              label="All-session checkpoints"
              value={String(status()!.checkpoints_total)}
              tone="muted"
            />
          </box>
        </box>
      ) : null}

      {/* Optional semantic build progress — full-width below the columns */}
      {status()?.semantic_index.stage ? (
        <box flexDirection="column" width="100%" marginTop={1}>
          <text fg={t().text}>
            <b>Semantic build progress</b>
          </text>
          <R theme={t()} label="Stage" value={status()!.semantic_index.stage!} />
          {status()!.semantic_index.files != null ? (
            <R
              theme={t()}
              label="Files seen"
              value={formatCountShort(status()!.semantic_index.files)}
            />
          ) : null}
          {status()!.semantic_index.entries_done != null ||
          status()!.semantic_index.entries_total != null ? (
            <R
              theme={t()}
              label="Progress"
              value={`${formatCountShort(status()!.semantic_index.entries_done ?? null)} / ${formatCountShort(status()!.semantic_index.entries_total ?? null)}`}
            />
          ) : null}
        </box>
      ) : null}

      {/* Semantic error — full-width, themed error color */}
      {status()?.semantic_index.error ? (
        <box marginTop={1} width="100%">
          <text fg={t().error}>⚠ {status()!.semantic_index.error}</text>
        </box>
      ) : null}

      {/* Compression aggregates — tabular layout matching Search/Semantic
          Index. Each scope ("Session", "Project") renders as a subheader
          followed by two <R> rows (Tokens Saved, Compression Ratio) so the
          numbers stay right-aligned under the value column instead of
          crowding the label. */}
      {compressionAggregateRows().length > 0 ? (
        <box flexDirection="column" width="100%" marginTop={1}>
          <text fg={t().text}>
            <b>Compression</b>
          </text>
          {compressionAggregateRows().map((row) =>
            row.kind === "scope" ? (
              <box width="100%">
                <text fg={t().text}>{row.label}</text>
              </box>
            ) : (
              <R theme={t()} label={row.label} value={row.value} tone="muted" />
            ),
          )}
        </box>
      ) : null}

      {/* Human health values are optional. Omit a category until the server
          proves it rather than displaying an unproven zero as clean. */}
      {status()?.status_bar ? (
        <box flexDirection="column" width="100%" marginTop={1}>
          <text fg={t().text}>
            <b>{status()!.status_bar!.tier2_stale ? "Code Health ~" : "Code Health"}</b>
          </text>
          {status()!.status_bar!.errors !== undefined ? (
            <R
              theme={t()}
              label="Errors"
              value={formatCountShort(status()!.status_bar!.errors)}
              tone={status()!.status_bar!.errors! > 0 ? "err" : "muted"}
            />
          ) : null}
          {status()!.status_bar!.warnings !== undefined ? (
            <R
              theme={t()}
              label="Warnings"
              value={formatCountShort(status()!.status_bar!.warnings)}
              tone={status()!.status_bar!.warnings! > 0 ? "warn" : "muted"}
            />
          ) : null}
          {status()!.status_bar!.dead_code !== undefined ? (
            <R
              theme={t()}
              label="Dead Code"
              value={formatCountShort(status()!.status_bar!.dead_code)}
              tone="muted"
            />
          ) : null}
          {status()!.status_bar!.unused_exports !== undefined ? (
            <R
              theme={t()}
              label="Unused Exports"
              value={formatCountShort(status()!.status_bar!.unused_exports)}
              tone="muted"
            />
          ) : null}
          {status()!.status_bar!.duplicates !== undefined ? (
            <R
              theme={t()}
              label="Duplicates"
              value={formatCountShort(status()!.status_bar!.duplicates)}
              tone="muted"
            />
          ) : null}
          {status()!.status_bar!.todos !== undefined ? (
            <R
              theme={t()}
              label="TODOs"
              value={formatCountShort(status()!.status_bar!.todos)}
              tone="muted"
            />
          ) : null}
        </box>
      ) : null}

      {/* Footer */}
      <box marginTop={1} justifyContent="flex-end" width="100%">
        <text fg={t().textMuted}>Enter or Esc to close</text>
      </box>
    </box>
  );
};

async function showStatusDialog(api: TuiPluginApi): Promise<void> {
  const sessionID = getSessionId(api);
  if (!sessionID) {
    api.ui.toast({ message: "No active session", variant: "warning", duration: 5000 });
    return;
  }

  const directory = api.state.path.directory ?? "";
  if (!directory) {
    api.ui.toast({ message: "No project directory", variant: "warning", duration: 5000 });
    return;
  }

  const client = getRpcClient(directory);

  // Prime the dialog with one initial fetch so we don't show a blank
  // skeleton — the component then listens for pushed invalidations.
  let initial: AftStatusSnapshot | null = null;
  let initialError: string | null = null;
  try {
    const response = await client.call(
      "status",
      { sessionID },
      { accept: statusAcceptGate(directory) },
    );
    if ((response as Record<string, unknown>).success !== false) {
      initial = coerceAftStatus(response as Record<string, unknown>);
    } else {
      initialError = "AFT bridge returned an error response.";
    }
  } catch {
    initialError = "AFT is starting up. Status will refresh automatically...";
  }

  // The dialog host already wraps every replace()'d element in its own centered
  // frame and binds Esc/Ctrl-C to close it. Rendering our own dialog frame inside
  // that host frame would misplace the content and focus; pass the bare component
  // so the host frames it once.
  // Do not pass an onClose that calls dialog.clear(). The dialog system calls
  // every entry's onClose on Esc, and clear() would re-trigger them, causing
  // infinite recursion. The host itself pops the dialog; StatusDialog cleans up
  // its own subscriptions in onCleanup. `replace` resets size to "medium", so
  // request "large" after the call.
  api.ui.dialog.replace(() => (
    <StatusDialog
      api={api}
      client={client}
      directory={directory}
      sessionID={sessionID}
      initial={initial}
      initialError={initialError}
    />
  ));
  api.ui.dialog.setSize("large");
}

/**
 * Register the AFT status palette command, preferring the v1.14.42+ keymap API
 * and falling back to the legacy `api.command.register` for older hosts.
 *
 * No slash entry is registered here. The server config hook owns the single
 * `/aft-status` slash registration, while this palette command keeps Ctrl+P
 * discovery in the TUI.
 */
function registerStatusCommand(api: TuiPluginApi): void {
  type ApiAny = {
    keymap?: {
      registerLayer?: (layer: {
        commands: Array<Record<string, unknown>>;
        bindings: Array<Record<string, unknown>>;
      }) => unknown;
    };
    command?: {
      register?: (cb: () => Array<Record<string, unknown>>) => unknown;
    };
  };
  const apiAny = api as unknown as ApiAny;

  if (typeof apiAny.keymap?.registerLayer === "function") {
    apiAny.keymap.registerLayer({
      commands: [
        {
          namespace: "palette",
          name: "aft.status",
          title: "AFT: Status",
          category: "AFT",
          run() {
            void showStatusDialog(api);
          },
        },
      ],
      bindings: [],
    });
    return;
  }

  if (typeof apiAny.command?.register === "function") {
    apiAny.command.register(() => [
      {
        title: "AFT: Status",
        value: "aft.status",
        category: "AFT",
        onSelect() {
          void showStatusDialog(api);
        },
      },
    ]);
    return;
  }

  // Neither API surface is present. The TUI host can still load; users can
  // still see AFT status through the sidebar and server-owned slash command.
}

async function showStartupNotifications(api: TuiPluginApi): Promise<void> {
  const directory = api.state.path.directory ?? "";
  if (!directory) return;

  const client = getRpcClient(directory);

  // Check for feature announcements
  try {
    const announcement = (await client.call("get-announcement", {})) as {
      show?: boolean;
      version?: string;
      features?: string[];
      footer?: string;
    };

    if (announcement.show && announcement.version && announcement.features?.length) {
      const featureText = announcement.features.map((f: string) => `  • ${f}`).join("\n");
      // Blank-line separator distinguishes the persistent footer (Discord
      // invite, etc.) from the version-specific bullets.
      const hasFooter =
        typeof announcement.footer === "string" && announcement.footer.trim().length > 0;
      const message = hasFooter
        ? `What's new:\n\n${featureText}\n\n${announcement.footer}`
        : `What's new:\n\n${featureText}`;

      api.ui.dialog.replace(
        () => (
          <api.ui.DialogAlert
            title={`AFT v${announcement.version}`}
            message={message}
            onConfirm={() => {
              // Mark as announced so it doesn't show again
              void client.call("mark-announced", {});
            }}
          />
        ),
        () => {
          void client.call("mark-announced", {});
        },
      );
      return; // Show one dialog at a time
    }
  } catch {
    // RPC server not ready yet — skip announcements
  }

  // Check for warnings
  try {
    const result = (await client.call("get-warnings", {})) as { warnings?: string[] };
    if (result.warnings?.length) {
      const warningText = result.warnings.join("\n\n");
      api.ui.dialog.replace(
        () => <api.ui.DialogAlert title="AFT Warning" message={warningText} onConfirm={() => {}} />,
        () => {},
      );
    }
  } catch {
    // RPC server not ready — skip warnings
  }
}

const tui: TuiPlugin = async (api) => {
  // Sidebar slot: live status of search index, semantic index, and disk
  // usage. See ./sidebar.tsx for the panel itself. Registered before the
  // command palette entry so the sidebar is available immediately when the
  // user opens their first session.
  try {
    api.slots.register(await createAftSidebarSlot(api, packageVersion));
  } catch {
    // Older OpenCode TUI hosts may not implement api.slots; fall through and
    // keep the palette command / server-owned slash command working.
  }

  // OpenCode 1.14.42 removed `api.command.register` entirely
  // (anomalyco/opencode PR #26053). Later patches reinstated it as a
  // deprecated shim that translates to `api.keymap.registerLayer`. To work
  // across the whole 1.14.x line — including the brief 1.14.42 / 1.14.43
  // window where neither the legacy API nor a shim was present — we prefer
  // `api.keymap.registerLayer` and fall back to `api.command.register` only
  // when the keymap surface is missing (older hosts that predate keymap).
  // See https://github.com/cortexkit/aft/issues/33.
  registerStatusCommand(api);

  // Receive server → TUI dialog requests and status invalidations over one
  // persistent WebSocket. This avoids repeated loopback HTTP connection setup,
  // which caused idle CPU usage.
  const handleNotification = async (message: SocketNotification): Promise<boolean> => {
    const requestedSessionId = getSessionId(api);
    if (!requestedSessionId) return false;
    if (message.sessionId && message.sessionId !== requestedSessionId) return false;
    if (message.type !== "action") return false;
    if (message.payload?.action !== "show-status-dialog") return false;
    await showStatusDialog(api);
    return true;
  };

  startAftTuiSocket({
    getDirectory: () => api.state.path.directory ?? "",
    getSessionId: () => getSessionId(api),
    onNotification: handleNotification,
  });

  const socketScopeUnsubs = [
    api.event?.on?.("message.updated", () => refreshAftTuiSocketScope()),
    api.event?.on?.("session.updated", () => refreshAftTuiSocketScope()),
  ].filter(Boolean);

  api.lifecycle?.onDispose?.(() => {
    stopAftTuiSocket();
    for (const unsub of socketScopeUnsubs) {
      try {
        unsub();
      } catch {
        // Ignore unsubscribe errors during cleanup; socket shutdown is handled separately.
      }
    }
    for (const client of rpcClients.values()) client.reset();
    rpcClients.clear();
  });

  // Show startup notifications — RPC server is already running by the time TUI loads
  void showStartupNotifications(api);
};

const id = "aft-opencode";

export default {
  id,
  tui,
  setup: setupV2Tui,
};
