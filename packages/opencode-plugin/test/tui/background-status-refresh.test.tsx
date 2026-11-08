/** @jsxImportSource @opentui/solid */

// The daemon pushes a status snapshot whenever its own state changes (an
// index finishing, a Tier-2 count arriving). Those pushes answer no session's
// request, so the daemon strips the session-scoped fields from them
// (`status_push_payload` in crates/aft/src/protocol.rs). This follows such a
// push through the plugin's RPC registration into the OpenCode 2 sidebar and
// footer refresh for an empty project, whose semantic index finishes with zero
// entries: the TUI must end on ready, 0 entries and the real disk size, and a
// build still in progress must keep saying loading.

import { describe, expect, test } from "bun:test";
import type { AftTransportPool } from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import { type AftRpcContext, registerAftRpc } from "../../src/rpc/register.js";
import { __resetRpcNotificationsForTest } from "../../src/shared/rpc-notifications.js";
import { coerceAftStatus, formatSemanticIndexLabel } from "../../src/shared/status.js";
import { subscribeV2StatusRefresh } from "../../src/tui/v2.js";
import { formatAftStatusSegment } from "../../src/tui/v2-status.js";

type EventName = "statusInvalidated" | "showStatusDialog" | "indexProgress";
type EventHandler = (event: { data: { sessionID?: string } }) => void | Promise<void>;

const TUI_SESSION = "ses_tui";

/**
 * A status snapshot in the shape the daemon sends for an empty project (one
 * `git init` folder, nothing else). The ready values are the ones a local
 * standalone run reported once `build_ready plane=semantic` had been logged:
 * 0 entries and a 216-byte persisted index.
 */
function emptyProjectSnapshot(
  sessionID: string | null,
  semantic: "loading" | "ready",
): Record<string, unknown> {
  const snapshot: Record<string, unknown> = {
    success: true,
    version: "0.58.0",
    project_root: "/work/empty",
    canonical_root: "/work/empty",
    cache_role: "main",
    degraded: false,
    degraded_reasons: [],
    features: { search_index: true, semantic_search: true, callgraph_store: true },
    search_index: { status: "ready", files: 0, trigrams: 0 },
    semantic_index:
      semantic === "ready"
        ? {
            status: "ready",
            state: "ready",
            refreshing_count: 0,
            entries: 0,
            dimension: 384,
            backend: "fastembed",
            model: "all-MiniLM-L6-v2",
          }
        : {
            status: "loading",
            state: "loading",
            refreshing_count: 0,
            stage: "loading_model",
            files: null,
            entries_done: null,
            entries_total: null,
          },
    disk: {
      storage_dir: "/cache",
      trigram_disk_bytes: 197,
      semantic_disk_bytes: semantic === "ready" ? 216 : 0,
    },
    status_bar_values: {
      errors: null,
      warnings: null,
      diagnostics: "no_language_server",
      todos: 0,
    },
  };
  // A background push carries no session; a direct answer carries the asker's.
  if (sessionID !== null) snapshot.session = { id: sessionID, tracked_files: 0, checkpoints: 0 };
  return snapshot;
}

/**
 * The daemon side of one project: a direct `status` request answers for the
 * session that asked, and `push` delivers a background `status_changed`
 * snapshot to subscribers the way BinaryBridge does.
 */
class DaemonBridge {
  semantic: "loading" | "ready" = "loading";
  private cached: Record<string, unknown> | null = null;
  private listeners = new Set<(snapshot: Record<string, unknown>) => void>();

  getCwd(): string {
    return "/work/empty";
  }

  getCachedStatus(): Record<string, unknown> | null {
    return this.cached;
  }

  cacheStatusSnapshot(snapshot: Record<string, unknown>): void {
    this.cached = snapshot;
  }

  async send(_command: string, params?: Record<string, unknown>): Promise<Record<string, unknown>> {
    return emptyProjectSnapshot(String(params?.session_id ?? "__default__"), this.semantic);
  }

  async toolCall(): Promise<never> {
    throw new Error("unused");
  }

  subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void {
    this.listeners.add(listener);
    if (this.cached) listener(this.cached);
    return () => this.listeners.delete(listener);
  }

  /** The daemon finished a background build and pushed its new state. */
  finishSemanticBuild(): void {
    this.semantic = "ready";
    this.push(emptyProjectSnapshot(null, "ready"));
  }

  /** The daemon pushed a progress update for a build that is still running. */
  pushProgress(): void {
    this.push(emptyProjectSnapshot(null, "loading"));
  }

  private push(snapshot: Record<string, unknown>): void {
    this.cached = snapshot;
    for (const listener of this.listeners) listener(snapshot);
  }
}

function pool(bridge: DaemonBridge): AftTransportPool {
  return {
    getBridge: () => bridge,
    getActiveBridgeForRoot: () => bridge,
    activeBridges: () => [bridge],
    toolCall: async () => ({ success: true, text: "ok" }),
    setConfigureOverride: () => {},
    reconfigure: async () => {},
    replaceBinary: async (path: string) => path,
    isShutdown: () => false,
    shutdown: async () => {},
    closeSession: async () => {},
  } as unknown as AftTransportPool;
}

/**
 * The host between the plugin and the TUI: what the plugin emits is delivered
 * to the TUI's typed event handlers, and the TUI's getStatus reaches the
 * plugin's handler.
 */
function host() {
  const handlers = new Map<EventName, Set<EventHandler>>();
  let getStatus:
    | ((input: { sessionID?: string }) => Effect.Effect<Record<string, unknown>, unknown>)
    | undefined;
  const pending: Array<Promise<unknown>> = [];
  const context: AftRpcContext = {
    rpc: {
      register(_definition, registered) {
        return Effect.sync(() => {
          getStatus = registered.getStatus;
          return {
            events: {
              emit(name, payload) {
                return Effect.sync(() => {
                  for (const handler of handlers.get(name) ?? []) {
                    pending.push(Promise.resolve(handler({ data: payload })));
                  }
                });
              },
            },
            dispose: Effect.void,
          };
        });
      },
    },
  };
  const tuiRpc = {
    getStatus(input: { sessionID?: string }) {
      if (!getStatus) throw new Error("getStatus was not registered");
      return Effect.runPromise(getStatus(input));
    },
    events: {
      on(name: EventName, handler: EventHandler) {
        let set = handlers.get(name);
        if (!set) {
          set = new Set();
          handlers.set(name, set);
        }
        set.add(handler);
        return () => set?.delete(handler);
      },
    },
  };
  const settle = async () => {
    for (let round = 0; round < 5; round++) {
      await Promise.all(pending.splice(0));
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  };
  return { context, tuiRpc, settle };
}

describe("background status pushes reach the OpenCode 2 sidebar and footer", () => {
  test("an empty project's finished semantic index replaces the loading snapshot", async () => {
    __resetRpcNotificationsForTest();
    const bridge = new DaemonBridge();
    const h = host();
    const registered = await Effect.runPromise(
      Effect.scoped(registerAftRpc(h.context, { directory: "/work/empty" }, pool(bridge))),
    );

    // What the TUI's useAftStatus does: fetch once, then fetch again on every
    // invalidation or progress event for its session.
    let shown: Record<string, unknown> | null = null;
    let fetches = 0;
    const refresh = async () => {
      fetches += 1;
      shown = await h.tuiRpc.getStatus({ sessionID: TUI_SESSION });
    };
    const unsubscribe = subscribeV2StatusRefresh(
      h.tuiRpc as never,
      () => TUI_SESSION,
      () => {
        void refresh();
      },
    );
    await refresh();
    expect(coerceAftStatus(shown!).semantic_index.status).toBe("loading");
    const fetchesWhileLoading = fetches;

    bridge.finishSemanticBuild();
    await h.settle();

    expect(fetches).toBeGreaterThan(fetchesWhileLoading);
    const status = coerceAftStatus(shown!);
    expect(status.semantic_index.status).toBe("ready");
    expect(formatSemanticIndexLabel(status.semantic_index)).toBe("ready");
    expect(status.semantic_index.entries).toBe(0);
    expect(status.disk.semantic_disk_bytes).toBe(216);
    expect(status.search_index.files).toBe(0);
    expect(formatAftStatusSegment(status)).toBe("AFT no LSP | T0");

    unsubscribe();
    await registered.dispose();
  });

  test("a build still in progress keeps saying loading after a push", async () => {
    __resetRpcNotificationsForTest();
    const bridge = new DaemonBridge();
    const h = host();
    const registered = await Effect.runPromise(
      Effect.scoped(registerAftRpc(h.context, { directory: "/work/empty" }, pool(bridge))),
    );
    let shown: Record<string, unknown> | null = null;
    const unsubscribe = subscribeV2StatusRefresh(
      h.tuiRpc as never,
      () => TUI_SESSION,
      () => {
        void h.tuiRpc.getStatus({ sessionID: TUI_SESSION }).then((next) => {
          shown = next;
        });
      },
    );

    // A progress push while the model is still loading: the TUI refreshes and
    // must show the honest in-progress state, not ready.
    bridge.pushProgress();
    await h.settle();

    expect(shown).not.toBeNull();
    const status = coerceAftStatus(shown!);
    expect(status.semantic_index.status).toBe("loading");
    expect(status.disk.semantic_disk_bytes).toBe(0);

    unsubscribe();
    await registered.dispose();
  });
});
