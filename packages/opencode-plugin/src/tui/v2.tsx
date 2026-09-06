/** @jsxImportSource @opentui/solid */
// @ts-nocheck

import { createEffect, createSignal, onCleanup } from "solid-js";
import {
  type AftStatusSnapshot,
  coerceAftStatus,
  formatStatusDialogMessage,
} from "../shared/status";
import { AftRpc } from "./aft-rpc";
import { formatAftStatusSegment, summarizeAftSidebar } from "./v2-status";

type AftRpcClient = {
  getStatus(
    input: { sessionID?: string },
    options?: { location?: unknown; signal?: AbortSignal },
  ): Promise<Record<string, unknown>>;
  events: {
    on(
      name: "statusInvalidated" | "showStatusDialog" | "indexProgress",
      handler: (event: { data: { sessionID?: string } }) => void | Promise<void>,
      options?: { signal?: AbortSignal },
    ): () => void;
  };
};

type V2TuiContext = {
  location?: unknown;
  client: { rpc(definition: typeof AftRpc): AftRpcClient };
  keymap: {
    layer(input: () => { commands: Array<Record<string, unknown>>; bindings: string[] }): void;
  };
  ui: {
    dialog: { alert(input: { title: string; message: string }): Promise<void> };
    router: { current(): { type: string; sessionID?: string } };
    slot(claim: {
      append: "prompt.footer.status" | "sidebar.content";
      render(input: { sessionID?: string }): unknown;
    }): () => void;
  };
};

function eventMatchesSession(
  eventSessionID: string | undefined,
  sessionID: string | undefined,
): boolean {
  return eventSessionID === undefined || eventSessionID === sessionID;
}

async function fetchStatus(
  context: V2TuiContext,
  rpc: AftRpcClient,
  sessionID: string | undefined,
  signal?: AbortSignal,
): Promise<AftStatusSnapshot> {
  const response = await rpc.getStatus(sessionID ? { sessionID } : {}, {
    ...(context.location ? { location: context.location } : {}),
    ...(signal ? { signal } : {}),
  });
  return coerceAftStatus(response);
}

export function subscribeV2StatusRefresh(
  rpc: AftRpcClient,
  sessionID: () => string | undefined,
  refresh: () => void,
): () => void {
  const unsubscribes = [
    rpc.events.on("statusInvalidated", (event) => {
      if (eventMatchesSession(event.data.sessionID, sessionID())) refresh();
    }),
    rpc.events.on("indexProgress", (event) => {
      if (eventMatchesSession(event.data.sessionID, sessionID())) refresh();
    }),
  ];
  return () => {
    for (const unsubscribe of unsubscribes) unsubscribe();
  };
}

function useAftStatus(
  context: V2TuiContext,
  rpc: AftRpcClient,
  sessionID: () => string | undefined,
): () => AftStatusSnapshot | null {
  const [status, setStatus] = createSignal<AftStatusSnapshot | null>(null);
  let refreshGeneration = 0;
  let refreshController: AbortController | undefined;

  const refresh = async (): Promise<void> => {
    const requestedSessionID = sessionID();
    const generation = ++refreshGeneration;
    refreshController?.abort();
    const controller = new AbortController();
    refreshController = controller;
    try {
      const next = await fetchStatus(context, rpc, requestedSessionID, controller.signal);
      if (controller.signal.aborted || generation !== refreshGeneration) return;
      if (sessionID() !== requestedSessionID) return;
      setStatus(next);
    } catch {
      // Status is best-effort UI data; the next typed invalidation retries it.
    }
  };

  createEffect(() => {
    sessionID();
    void refresh();
  });
  const unsubscribe = subscribeV2StatusRefresh(rpc, sessionID, () => {
    void refresh();
  });
  onCleanup(() => {
    refreshGeneration += 1;
    refreshController?.abort();
    unsubscribe();
  });
  return status;
}

function FooterStatus(props: { context: V2TuiContext; rpc: AftRpcClient; sessionID?: string }) {
  const status = useAftStatus(props.context, props.rpc, () => props.sessionID);
  return <text>{formatAftStatusSegment(status())}</text>;
}

function SidebarStatus(props: { context: V2TuiContext; rpc: AftRpcClient; sessionID: string }) {
  const status = useAftStatus(props.context, props.rpc, () => props.sessionID);
  const summary = () => summarizeAftSidebar(status());
  return (
    <box width="100%" flexDirection="column">
      <text>
        <b>{summary().title}</b>
        {summary().version ? ` v${summary().version}` : ""}
      </text>
      <text>Search: {summary().search}</text>
      <text>Semantic: {summary().semantic}</text>
      <text>Health: {summary().health}</text>
    </box>
  );
}

function activeSessionID(context: V2TuiContext): string | undefined {
  const route = context.ui.router.current();
  return route.type === "session" ? route.sessionID : undefined;
}

async function showStatusDialog(
  context: V2TuiContext,
  rpc: AftRpcClient,
  sessionID: string | undefined,
): Promise<void> {
  try {
    const status = await fetchStatus(context, rpc, sessionID);
    await context.ui.dialog.alert({
      title: "AFT Status",
      message: formatStatusDialogMessage(status),
    });
  } catch {
    await context.ui.dialog.alert({
      title: "AFT Status",
      message: "AFT is starting up. Status will refresh automatically.",
    });
  }
}

export async function setupV2Tui(context: V2TuiContext): Promise<() => void> {
  const rpc = context.client.rpc(AftRpc);
  const controller = new AbortController();
  const slotCleanups = [
    context.ui.slot({
      append: "prompt.footer.status",
      render: (input) => <FooterStatus context={context} rpc={rpc} sessionID={input.sessionID} />,
    }),
    context.ui.slot({
      append: "sidebar.content",
      render: (input) => <SidebarStatus context={context} rpc={rpc} sessionID={input.sessionID!} />,
    }),
  ];

  const stopDialogEvents = rpc.events.on(
    "showStatusDialog",
    (event) => showStatusDialog(context, rpc, event.data.sessionID ?? activeSessionID(context)),
    { signal: controller.signal },
  );

  context.keymap.layer(() => ({
    commands: [
      {
        id: "aft.status",
        title: "AFT: Status",
        description: "Show AFT status, index health, and cache usage",
        group: "AFT",
        palette: true,
        // V2's KeymapCommand contract (plugin/tui/context.ts:379-385) dispatches
        // slash entries unless `arguments: true`; omitting it prevents this text
        // from remaining in the prompt or being submitted to the model.
        slash: { name: "aft-status" },
        run: () => showStatusDialog(context, rpc, activeSessionID(context)),
      },
    ],
    bindings: [],
  }));

  return () => {
    controller.abort();
    stopDialogEvents();
    for (const cleanup of slotCleanups) cleanup();
  };
}
