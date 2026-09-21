/** @jsxImportSource @opentui/solid */
// @ts-nocheck

import { createEffect, createSignal, onCleanup } from "solid-js";
import { version as packageVersion } from "../../package.json";
import {
  type AftStatusSnapshot,
  coerceAftStatus,
  formatStatusDialogMessage,
} from "../shared/status";
import { AftRpc } from "./aft-rpc";
import {
  AftSidebarPanel,
  createSidebarPreferences,
  type SidebarColor,
  type SidebarPalette,
} from "./sidebar-view";
import { formatAftStatusSegment } from "./v2-status";

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

/**
 * The colour tokens the sidebar needs out of the host's `ResolvedTheme` — the
 * type `Context.theme` carries, exported by `@opencode/theme/tui`. That
 * package is an optional peer of `@opencode/plugin` and is not installed
 * alongside this plugin, so the shape is written out here instead of imported;
 * the key names are the theme package's own. They are nested token groups,
 * unlike the flat colour names the slot-plugin host in ./sidebar.tsx reads.
 * Every level is optional so a host that ships a partial theme degrades to the
 * fallback palette below instead of throwing during a render.
 *
 * What a host really hands over is recorded, token for token, in
 * test/load-matrix/ga-host-theme.json; that record is what this type and the
 * mapping below are written against, and the load matrix compares it to a live
 * host on every run.
 */
type GaFeedbackKind = "error" | "warning" | "success" | "info";

/**
 * One step of a hue scale. The steps are ordered by how far the shade stands
 * off the page rather than by lightness: 100 contrasts with the page most and
 * 900 blends into it, in both theme modes. So the host's dark theme resolves
 * accent 100 to its lightest purple and its light theme resolves accent 100 to
 * its darkest orange, and a step chosen for one mode keeps its role in the
 * other.
 */
type GaHueStep = "100" | "200" | "300" | "400" | "500" | "600" | "700" | "800" | "900";

type GaResolvedTheme = {
  hue?: { accent?: Partial<Record<GaHueStep, SidebarColor>> };
  text?: {
    base?: SidebarColor;
    muted?: SidebarColor;
    feedback?: Partial<Record<GaFeedbackKind, { base?: SidebarColor }>>;
  };
  background?: { base?: SidebarColor };
  border?: { base?: SidebarColor };
};

/**
 * The accent shade the AFT badge is filled with. A low step is what keeps the
 * fill standing off the page in both theme modes (see GaHueStep), and 200 is
 * the step the host itself draws its own accent-coloured UI in.
 */
const BADGE_ACCENT_STEP: GaHueStep = "200";

type V2TuiContext = {
  location?: unknown;
  client: { rpc(definition: typeof AftRpc): AftRpcClient };
  /** Resolved for the active theme mode by the host; see GaResolvedTheme. */
  theme?: GaResolvedTheme;
  themeMode?: "dark" | "light";
  keymap: {
    layer(input: () => { commands: Array<Record<string, unknown>>; bindings: string[] }): void;
  };
  ui: {
    dialog: { alert(input: { title: string; message: string }): Promise<void> };
    router: { current(): { type: string; sessionID?: string } };
    slot(claim: {
      append: "app" | "prompt.footer.status" | "sidebar.content";
      render(input: { sessionID?: string }): unknown;
    }): () => void;
  };
};

// Last resort for a host that declares a theme mode but hands over no resolved
// theme. Inheriting the terminal default instead is what made the panel white
// on white in light mode, so there has to be a legible colour for every role
// even when the theme is missing.
const FALLBACK_PALETTES: Record<"dark" | "light", SidebarPalette> = {
  dark: {
    text: "#e6edf3",
    textMuted: "#9198a1",
    success: "#3fb950",
    warning: "#d29922",
    error: "#f85149",
    accent: "#1f6feb",
    background: "#0d1117",
    border: "#3d444d",
  },
  light: {
    text: "#1f2328",
    textMuted: "#59636e",
    success: "#1a7f37",
    warning: "#9a6700",
    error: "#cf222e",
    accent: "#0969da",
    background: "#ffffff",
    border: "#d1d9e0",
  },
};

/**
 * Host theme → the panel's palette. The host resolves its theme for the active
 * mode before handing it over, so light mode arrives as light colours here;
 * what matters is that every text line asks for one of them instead of
 * inheriting the terminal default.
 *
 * `success` falls back to the accent exactly as ./sidebar.tsx's mapping does.
 */
export function resolveV2Palette(
  theme: GaResolvedTheme | undefined,
  themeMode: "dark" | "light" | undefined,
): SidebarPalette {
  const fallback = FALLBACK_PALETTES[themeMode === "light" ? "light" : "dark"];
  if (!theme) return { ...fallback };

  // The accent comes out of the hue scales, not out of `background.action`.
  // The action groups describe how the host paints a button, and this theme
  // paints one as bare text: its `background.action.primary.base` is a fully
  // transparent colour. Filling the badge with that drew no badge at all, and
  // nothing caught it, because a present-but-transparent colour never reaches
  // the fallback below.
  const accent = theme.hue?.accent?.[BADGE_ACCENT_STEP] ?? fallback.accent;
  const feedback = theme.text?.feedback;
  return {
    text: theme.text?.base ?? fallback.text,
    textMuted: theme.text?.muted ?? fallback.textMuted,
    success: feedback?.success?.base ?? accent,
    warning: feedback?.warning?.base ?? fallback.warning,
    error: feedback?.error?.base ?? fallback.error,
    accent,
    background: theme.background?.base ?? fallback.background,
    // This theme has one border colour; ./sidebar.tsx's host distinguishes an
    // active one, and the panel only ever draws the active border.
    border: theme.border?.base ?? fallback.border,
  };
}

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
  const preferences = createSidebarPreferences();
  return (
    <AftSidebarPanel
      palette={resolveV2Palette(props.context.theme, props.context.themeMode)}
      snapshot={status()}
      prefs={preferences.prefs()}
      collapsed={preferences.collapsed()}
      onToggleCollapsed={preferences.toggleCollapsed}
      pluginVersion={packageVersion}
    />
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

// `keymap.layer` is a Solid primitive, not a plain registration call: it reads
// the host's keymap context and creates a layer owned by the component that
// calls it. The host hands that primitive to plugins unbound, and it runs the
// plugin's `setup` from a detached async continuation, outside both the Solid
// owner and the provider tree — so calling it from `setup` throws
// "Keymap.Provider is missing" and takes the whole TUI feature down with it.
// Rendering this component instead puts the call inside the provider tree,
// which is where the contract allows it. The component renders nothing; it
// exists only to own the layer, and is mounted through the `app` slot below.
function AftStatusCommands(props: { context: V2TuiContext; rpc: AftRpcClient }) {
  props.context.keymap.layer(() => ({
    // A layer without a mode is limited to the host's base input mode, and the
    // command palette runs as a dialog, which switches the mode away from base.
    // "global" opts out of that limit so the palette entry stays reachable
    // while the palette itself is open.
    mode: "global",
    commands: [
      {
        id: "aft.status",
        title: "AFT: Status",
        description: "Show AFT status, index health, and cache usage",
        group: "AFT",
        palette: true,
        // The host dispatches slash entries unless `arguments: true` is set;
        // omitting it prevents this text from remaining in the prompt or being
        // submitted to the model.
        slash: { name: "aft-status" },
        run: () => showStatusDialog(props.context, props.rpc, activeSessionID(props.context)),
      },
    ],
    bindings: [],
  }));
  return null;
}

export async function setupV2Tui(context: V2TuiContext): Promise<() => void> {
  const rpc = context.client.rpc(AftRpc);
  const controller = new AbortController();
  const slotCleanups = [
    // `app` is the one slot the host keeps mounted for the whole TUI session,
    // independent of the current route, so a command owned by a component
    // rendered here stays reachable everywhere the palette and slash
    // completion are.
    context.ui.slot({
      append: "app",
      render: () => <AftStatusCommands context={context} rpc={rpc} />,
    }),
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

  return () => {
    controller.abort();
    stopDialogEvents();
    for (const cleanup of slotCleanups) cleanup();
  };
}
