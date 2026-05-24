/** @jsxImportSource @opentui/solid */
// @ts-nocheck

import type { TuiPlugin, TuiPluginApi, TuiThemeCurrent } from "@opencode-ai/plugin/tui";
import { createMemo, createSignal, onCleanup } from "solid-js";

import packageJson from "../../package.json";
import { AftRpcClient } from "../shared/rpc-client";
import { type AftStatusSnapshot, coerceAftStatus, formatBytes } from "../shared/status";
import { createAftSidebarSlot, formatCompressionSidebarRows } from "./sidebar";

// The TUI talks to the server plugin via AftRpcClient. The client reads the
// JSON port file written by AftRpcServer ({ port, token }) and includes that
// per-server token on every RPC request; legacy integer port files are still
// tolerated for already-running older server plugins.

const STATUS_COMMAND = "aft-status";

// RPC clients keyed by directory — one per project
const rpcClients = new Map<string, AftRpcClient>();

function getRpcClient(directory: string): AftRpcClient {
  let client = rpcClients.get(directory);
  if (client) return client;

  // v0.27 moved AFT storage to the CortexKit root. The TUI plugin must use
  // the same path as the server plugin (`resolveCortexKitStorageRoot()`),
  // otherwise it polls stale legacy port files written by older versions
  // and never picks up the live RPC server, leaving the sidebar/dialog
  // stuck on the "AFT is starting up" placeholder forever.
  const home = process.env.HOME || process.env.USERPROFILE || "";
  const dataHome = process.env.XDG_DATA_HOME || `${home}/.local/share`;
  const storageDir = `${dataHome}/cortexkit/aft`;

  client = new AftRpcClient(storageDir, directory);
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
// StatusDialog — themed, two-column JSX dialog. Modeled on the magic-context
// /ctx-status pattern (packages/plugin/src/tui/index.tsx in that repo):
// custom JSX rendered via `api.ui.dialog.replace(() => <StatusDialog .../>)`
// instead of feeding a padded monospace string into DialogAlert. The
// difference matters because OpenCode renders DialogAlert text in a
// proportional font with no column alignment; only TUI flex primitives
// (<box flexDirection="row" flexBasis={0}>) actually produce visible
// columns. This component owns its own RPC polling so it can re-render
// reactively as the status snapshot changes, with no parent re-mount.
// ---------------------------------------------------------------------------

const POLL_INTERVAL_MS = 1500;

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
  sessionID: string;
  initial: AftStatusSnapshot | null;
  initialError: string | null;
  onClose: () => void;
}

const StatusDialog = (props: StatusDialogProps) => {
  const theme = createMemo(() => (props.api as any).theme.current as TuiThemeCurrent);
  const t = () => theme();

  // Reactive status signal — the dialog re-renders on every status
  // transition without remounting. The RPC polling is local to the dialog
  // and stops when it unmounts.
  const [status, setStatus] = createSignal<AftStatusSnapshot | null>(props.initial);
  const [error, setError] = createSignal<string | null>(props.initialError);

  const timer = setInterval(async () => {
    try {
      const response = await props.client.call("status", { sessionID: props.sessionID });
      if ((response as Record<string, unknown>).success !== false) {
        setStatus(coerceAftStatus(response as Record<string, unknown>));
        setError(null);
      }
    } catch {
      // transient — keep showing last good snapshot
    }
  }, POLL_INTERVAL_MS);
  onCleanup(() => clearInterval(timer));

  // Visual cache-role badge: main is accent, worktree is warning,
  // not_initialized is muted. Matches the sidebar convention.
  const cacheRoleTone = (role: string): "accent" | "warn" | "muted" =>
    role === "main" ? "accent" : role === "worktree" ? "warn" : "muted";
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
          <text fg={t().textMuted}>v{status()?.version ?? packageJson.version}</text>
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
            value={status()!.cache_role}
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
          </box>

          {/* Right column */}
          <box flexDirection="column" flexGrow={1} flexBasis={0}>
            <text fg={t().text}>
              <b>Semantic index</b>
            </text>
            <R
              theme={t()}
              label="Status"
              value={status()!.semantic_index.status}
              tone={statusTone(status()!.semantic_index.status)}
            />
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
  // skeleton — the component then takes over polling.
  let initial: AftStatusSnapshot | null = null;
  let initialError: string | null = null;
  try {
    const response = await client.call("status", { sessionID });
    if ((response as Record<string, unknown>).success !== false) {
      initial = coerceAftStatus(response as Record<string, unknown>);
    } else {
      initialError = "AFT bridge returned an error response.";
    }
  } catch {
    initialError = "AFT is starting up. Status will refresh automatically...";
  }

  api.ui.dialog.setSize("large");
  api.ui.dialog.replace(
    () => (
      <StatusDialog
        api={api}
        client={client}
        sessionID={sessionID}
        initial={initial}
        initialError={initialError}
        onClose={() => {
          api.ui.dialog.setSize("medium");
        }}
      />
    ),
    () => {
      api.ui.dialog.setSize("medium");
    },
  );
}

/**
 * Register the `/aft-status` slash command, preferring the v1.14.42+ keymap
 * API and falling back to the legacy `api.command.register` for older hosts.
 *
 * The `keymap.registerLayer` shape uses `name`/`title`/`run`/`namespace`/
 * `slashName` (see `@opencode-ai/plugin/tui` types) and is what the host's
 * own legacy command-shim translates into. Calling it directly skips the
 * deprecation warning and works without depending on the (now-deprecated)
 * `api.command` namespace existing at all.
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
          slashName: STATUS_COMMAND,
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
        slash: { name: STATUS_COMMAND },
        onSelect() {
          void showStatusDialog(api);
        },
      },
    ]);
    return;
  }

  // Neither API surface is present. The TUI host can still load — we only
  // lose the slash-command entry point. The sidebar (registered above)
  // remains available so users can still see AFT status visually.
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
    api.slots.register(createAftSidebarSlot(api, packageJson.version));
  } catch {
    // Older OpenCode TUI hosts may not implement api.slots; fall through
    // and keep the slash command working.
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

  // Show startup notifications — RPC server is already running by the time TUI loads
  void showStartupNotifications(api);
};

const id = "aft-opencode";

export default {
  id,
  tui,
};
