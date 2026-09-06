/** @jsxImportSource @opentui/solid */

import { describe, expect, test } from "bun:test";
import { AftRpc } from "../../src/rpc/contract.js";
import { setupV2Tui, subscribeV2StatusRefresh } from "../../src/tui/v2.js";

type EventName = "statusInvalidated" | "showStatusDialog" | "indexProgress";
type EventHandler = (event: { data: { sessionID?: string } }) => void | Promise<void>;

function statusResponse() {
  return {
    success: true,
    version: "0.55.1",
    project_root: "/work/project",
    canonical_root: "/work/project",
    cache_role: "main",
    degraded: false,
    degraded_reasons: [],
    features: {},
    search_index: { status: "ready", files: 12, trigrams: 34 },
    semantic_index: { status: "ready", refreshing_count: 0, entries: 10, dimension: 384 },
    disk: { storage_dir: "/cache", trigram_disk_bytes: 1, semantic_disk_bytes: 2 },
    lsp_servers: 0,
    runtime: { live_watchers: 1, live_actor_roots: 1, open_routes: 1 },
    symbol_cache: { local_entries: 1, warm_entries: 2 },
    storage_dir: "/cache",
    checkpoints_total: 0,
    session: { id: "ses_tui", tracked_files: 0, checkpoints: 0 },
    status_bar: {
      errors: 0,
      warnings: 0,
      dead_code: 0,
      unused_exports: 0,
      duplicates: 0,
      todos: 0,
    },
    message: "",
  };
}

function harness() {
  const claims: Array<{ append: string; render(input: { sessionID?: string }): unknown }> = [];
  const layers: Array<() => { commands: Array<Record<string, unknown>>; bindings: string[] }> = [];
  const alerts: Array<{ title: string; message: string }> = [];
  const handlers = new Map<EventName, Set<EventHandler>>();
  const calls: Array<{ input: unknown; options: unknown }> = [];
  let slotCleanups = 0;
  let eventCleanups = 0;

  const rpc = {
    async getStatus(input: unknown, options: unknown) {
      calls.push({ input, options });
      return statusResponse();
    },
    events: {
      on(name: EventName, handler: EventHandler) {
        let listeners = handlers.get(name);
        if (!listeners) {
          listeners = new Set();
          handlers.set(name, listeners);
        }
        listeners.add(handler);
        return () => {
          if (listeners?.delete(handler)) eventCleanups += 1;
        };
      },
    },
  };
  const context = {
    location: { id: "loc_1", directory: "/work/project" },
    client: {
      rpc(definition: unknown) {
        expect(definition).toBe(AftRpc);
        return rpc;
      },
    },
    keymap: {
      layer(input: (typeof layers)[number]) {
        layers.push(input);
      },
    },
    ui: {
      dialog: {
        async alert(input: { title: string; message: string }) {
          alerts.push(input);
        },
      },
      router: { current: () => ({ type: "session", sessionID: "ses_tui" }) },
      slot(claim: (typeof claims)[number]) {
        claims.push(claim);
        return () => {
          slotCleanups += 1;
        };
      },
    },
  };
  return {
    context,
    rpc,
    claims,
    layers,
    alerts,
    handlers,
    calls,
    slotCleanups: () => slotCleanups,
    eventCleanups: () => eventCleanups,
  };
}

async function emit(
  handlers: Map<EventName, Set<EventHandler>>,
  name: EventName,
  sessionID?: string,
): Promise<void> {
  await Promise.all(
    [...(handlers.get(name) ?? [])].map((handler) => handler({ data: { sessionID } })),
  );
  await Promise.resolve();
}

describe("OpenCode V2 TUI setup", () => {
  test("registers only the two ruled slots and the intercepted status command", async () => {
    const h = harness();
    const cleanup = await setupV2Tui(h.context as never);

    expect(h.claims.map(({ append }) => append)).toEqual([
      "prompt.footer.status",
      "sidebar.content",
    ]);
    expect(h.layers).toHaveLength(1);
    const layer = h.layers[0]!();
    expect(layer.bindings).toEqual([]);
    expect(layer.commands).toEqual([
      expect.objectContaining({
        id: "aft.status",
        palette: true,
        slash: { name: "aft-status" },
      }),
    ]);
    expect((layer.commands[0]!.slash as Record<string, unknown>).arguments).toBeUndefined();
    await (layer.commands[0]!.run as () => Promise<void>)();
    expect(h.alerts).toHaveLength(1);
    expect(h.calls[0]?.input).toEqual({ sessionID: "ses_tui" });

    cleanup();
    expect(h.slotCleanups()).toBe(2);
    expect(h.eventCleanups()).toBe(1);
  });

  test("opens the status dialog from the typed showStatusDialog event", async () => {
    const h = harness();
    const cleanup = await setupV2Tui(h.context as never);

    await emit(h.handlers, "showStatusDialog", "ses_tui");

    expect(h.calls).toEqual([
      {
        input: { sessionID: "ses_tui" },
        options: { location: h.context.location },
      },
    ]);
    expect(h.alerts).toHaveLength(1);
    expect(h.alerts[0]).toMatchObject({ title: "AFT Status" });
    expect(h.alerts[0]?.message).toContain("AFT version: 0.55.1");
    cleanup();
  });

  test("footer and sidebar refresh adapters consume typed invalidation and progress events", async () => {
    const h = harness();
    let refreshes = 0;
    const cleanup = subscribeV2StatusRefresh(
      h.rpc,
      () => "ses_tui",
      () => {
        refreshes += 1;
      },
    );

    await emit(h.handlers, "indexProgress", "ses_tui");
    await emit(h.handlers, "statusInvalidated", "ses_tui");
    await emit(h.handlers, "indexProgress", "ses_other");

    expect(refreshes).toBe(2);
    expect(h.handlers.get("indexProgress")?.size).toBe(1);
    expect(h.handlers.get("statusInvalidated")?.size).toBe(1);
    cleanup();
    expect(h.handlers.get("indexProgress")?.size).toBe(0);
    expect(h.handlers.get("statusInvalidated")?.size).toBe(0);
  });
});
