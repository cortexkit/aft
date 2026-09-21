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
import { type AftStatusSnapshot, coerceAftStatus } from "../shared/status";
import { resolveCortexKitStorageRoot } from "../shared/storage-paths";
import {
  createDebouncedStatusRefresh,
  refreshAftTuiSocketScope,
  subscribeStatusInvalidations,
} from "./notification-socket";
import {
  computeEffectiveOrder,
  DEFAULT_SLOT_ORDER,
  PLUGIN_KEY,
  readTuiPreferencesFile,
} from "./preferences";
import { AftSidebarPanel, createSidebarPreferences, type SidebarPalette } from "./sidebar-view";

// The panel itself lives in ./sidebar-view, so the newer host's entry
// (./v2.tsx) renders exactly the same components. These re-exports keep this
// module the import site it has always been for callers that only want the
// formatting helpers.
export {
  type CompressionRow,
  collapsedCompressionValue,
  collapsedHealthLights,
  degradedReasonLabel,
  formatCompressionSidebarRows,
  type HealthLights,
  type HealthLightTone,
} from "./sidebar-view";

const REFRESH_DEBOUNCE_MS = 200;

/**
 * Slot-plugin theme → the panel's palette. The host this file serves publishes
 * one flat colour per role (`theme.success`, `theme.textMuted`, ...), so the
 * mapping is a rename; `success` keeps its historical fall back to the accent
 * for themes that never defined it.
 */
export function resolveV1Palette(theme: TuiThemeCurrent): SidebarPalette {
  return {
    text: theme.text,
    textMuted: theme.textMuted,
    success: theme.success ?? theme.accent,
    warning: theme.warning,
    error: theme.error,
    accent: theme.accent,
    background: theme.background,
    border: theme.borderActive,
  };
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
  const preferences = createSidebarPreferences(() => requestRender());
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

  onCleanup(() => {
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

  return (
    <AftSidebarPanel
      palette={resolveV1Palette(props.theme)}
      snapshot={s()}
      prefs={preferences.prefs()}
      collapsed={preferences.collapsed()}
      onToggleCollapsed={preferences.toggleCollapsed}
      pluginVersion={props.pluginVersion}
    />
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
