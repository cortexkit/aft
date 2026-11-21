/** @jsxImportSource @opentui/solid */

import { describe, expect, mock, test } from "bun:test";
import type { DialogConfirmOptions, DialogSelectOptions } from "@opencode/plugin/tui/context";
import { AftRpc } from "../../src/rpc/contract.js";
import type { BackgroundTasksRpc } from "../../src/tui/background-tasks.js";
import { setupV2Tui, subscribeV2StatusRefresh } from "../../src/tui/v2.js";

type EventName = "statusInvalidated" | "showStatusDialog" | "indexProgress" | "hostToolOverlap";
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
      todos: 0,
    },
    message: "",
  };
}

function harness() {
  const claims: Array<{ append: string; render(input: { sessionID?: string }): unknown }> = [];
  const layers: Array<
    () => { mode?: string; commands: Array<Record<string, unknown>>; bindings: string[] }
  > = [];
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
  test("claims its three slots at setup and owns its commands from the app slot", async () => {
    const h = harness();
    const cleanup = await setupV2Tui(h.context as never);

    expect(h.claims.map(({ append }) => append)).toEqual([
      "app",
      "prompt.footer.status",
      "sidebar.content",
    ]);

    // The host's `keymap.layer` is a component-scoped primitive: it is only
    // legal from inside the host's component tree, which a plugin reaches by
    // rendering into a slot. Registering during setup throws instead, so the
    // command must not appear until the app slot actually renders.
    expect(h.layers).toHaveLength(0);
    const appClaim = h.claims.find(({ append }) => append === "app");
    expect(appClaim).toBeDefined();
    appClaim?.render({});

    expect(h.layers).toHaveLength(1);
    const layer = h.layers[0]!();
    expect(layer.mode).toBe("global");
    expect(layer.bindings).toEqual([]);
    expect(layer.commands).toEqual([
      expect.objectContaining({
        id: "aft.status",
        palette: true,
        slash: { name: "aft-status" },
      }),
      expect.objectContaining({
        id: "aft.tasks",
        palette: true,
        slash: { name: "aft-tasks" },
      }),
    ]);
    expect((layer.commands[0]!.slash as Record<string, unknown>).arguments).toBeUndefined();
    await (layer.commands[0]!.run as () => Promise<void>)();
    expect(h.alerts).toHaveLength(1);
    expect(h.calls[0]?.input).toEqual({ sessionID: "ses_tui" });

    cleanup();
    expect(h.slotCleanups()).toBe(3);
    // The status dialog and the host tool overlap toast subscriptions.
    expect(h.eventCleanups()).toBe(2);
  });

  test("lists and refreshes session tasks, then reports each stop separately from reloading", async () => {
    const h = harness();
    const getBackgroundTasks = mock<BackgroundTasksRpc["getBackgroundTasks"]>()
      .mockResolvedValueOnce([{ id: "task_before", command: "sleep 20" }])
      .mockResolvedValueOnce([
        { id: "task_finished", command: "sleep 40", workdir: "/work/project", elapsedMs: 1250 },
      ])
      .mockResolvedValueOnce([{ id: "task_stopped" }])
      .mockRejectedValueOnce(new Error("private backend details"));
    const stopBackgroundTask = mock<BackgroundTasksRpc["stopBackgroundTask"]>()
      .mockResolvedValueOnce({ outcome: "finished" })
      .mockResolvedValueOnce({ outcome: "stopped" });
    const choices = [0, 1, 1, 1]; // Refresh, select the first task, then cancel and retry the second.
    const select = mock(async (picker: DialogSelectOptions<unknown>) => {
      const row = choices.shift();
      return row === undefined ? undefined : picker.options[row]?.value;
    });
    const confirm = mock<(confirmation: DialogConfirmOptions) => Promise<boolean>>()
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(true);
    let route: { type: string; sessionID?: string } = { type: "home" };
    const cleanup = await setupV2Tui({
      ...h.context,
      client: { rpc: () => ({ ...h.rpc, getBackgroundTasks, stopBackgroundTask }) },
      ui: {
        ...h.context.ui,
        dialog: { ...h.context.ui.dialog, select, confirm },
        router: { current: () => route },
      },
    } as never);
    h.claims.find(({ append }) => append === "app")?.render({});
    const command = h.layers[0]!().commands.find(({ id }) => id === "aft.tasks");
    const run = command!.run as () => Promise<void>;

    await run();
    expect(h.alerts[0]?.message).toBe("Open a session to view its background tasks.");
    expect(getBackgroundTasks).not.toHaveBeenCalled();

    route = { type: "session", sessionID: "ses_tui" };
    await run();
    const pickers = select.mock.calls.map(([picker]) => picker);
    expect(pickers[0]?.title).toBe("AFT Background Tasks");
    expect(pickers[0]?.options.map(({ title }) => title)).toEqual(["Refresh", "sleep 20"]);
    expect(pickers[1]?.options[1]).toMatchObject({
      title: "sleep 40",
      description: "Working directory: /work/project",
      footer: "Elapsed: 1.3 s",
    });
    expect(confirm.mock.calls[0]?.[0]).toMatchObject({
      label: { confirm: "Stop" },
      message: "Command: sleep 40\nTask ID: task_finished",
    });
    expect(pickers[2]?.title).toBe("Background task finished");
    expect(pickers[2]?.options[1]?.title).toBe("Task task_stopped");
    expect(pickers[3]?.title).toBe("AFT Background Tasks");
    expect(pickers[4]?.title).toBe("Background task stopped");
    expect(pickers[4]?.options).toEqual([
      { title: "Refresh", value: "refresh", description: "Could not load background tasks" },
    ]);
    const location = { location: h.context.location };
    expect(getBackgroundTasks.mock.calls).toEqual(
      Array(4).fill([{ sessionID: "ses_tui" }, location]),
    );
    expect(stopBackgroundTask.mock.calls).toEqual([
      [{ sessionID: "ses_tui", taskID: "task_finished" }, location],
      [{ sessionID: "ses_tui", taskID: "task_stopped" }, location],
    ]);
    cleanup();
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
