/** @jsxImportSource @opentui/solid */
// Renders the shared AFT sidebar panel with OpenTUI's test renderer and prints
// the character frame. Run as its own process with the Solid transform
// preloaded (`bun --preload @opentui/solid/preload`): other test files replace
// solid-js and the OpenTUI JSX runtime with stubs for the whole test process,
// and a real render needs the real ones.
//
// Input (environment): AFT_SIDEBAR_SNAPSHOT is the status snapshot as JSON,
// AFT_SIDEBAR_WIDTH the panel width in columns, AFT_SIDEBAR_COLLAPSED selects
// the compact view, and AFT_SIDEBAR_PALETTE overrides the default theme roles.
// AFT_SIDEBAR_CAPTURE=spans includes cell colors alongside the character frame.
// AFT_SIDEBAR_PANEL_MODULE optionally selects a candidate module for comparisons.

import { testRender } from "@opentui/solid";
import { DEFAULT_PREFS } from "../../tui/preferences.ts";
import { AftSidebarPanel } from "../../tui/sidebar-view.tsx";

const Panel = process.env.AFT_SIDEBAR_PANEL_MODULE
  ? (await import(process.env.AFT_SIDEBAR_PANEL_MODULE)).AftSidebarPanel
  : AftSidebarPanel;
const snapshot = JSON.parse(process.env.AFT_SIDEBAR_SNAPSHOT ?? "null");
const width = Number(process.env.AFT_SIDEBAR_WIDTH ?? "38");
const palette = {
  text: "#ffffff",
  textMuted: "#888888",
  success: "#00ff00",
  warning: "#ffff00",
  error: "#ff0000",
  accent: "#0088ff",
  background: "#000000",
  border: "#444444",
  ...JSON.parse(process.env.AFT_SIDEBAR_PALETTE ?? "{}"),
};

const setup = await testRender(
  () => (
    <Panel
      palette={palette}
      snapshot={snapshot}
      prefs={structuredClone(DEFAULT_PREFS)}
      collapsed={process.env.AFT_SIDEBAR_COLLAPSED === "true"}
      onToggleCollapsed={() => {}}
      pluginVersion="0.0.0"
    />
  ),
  { width, height: 80 },
);
try {
  // This opt-in delay simulates a slow renderer before its first frame.
  const renderDelayMs = Number(process.env.AFT_SIDEBAR_TEST_RENDER_DELAY_MS ?? "0");
  if (renderDelayMs > 0) {
    await new Promise((resolve) => setTimeout(resolve, renderDelayMs));
  }
  await setup.renderOnce();
  process.stdout.write(
    process.env.AFT_SIDEBAR_CAPTURE === "spans"
      ? JSON.stringify({ frame: setup.captureCharFrame(), spans: setup.captureSpans() })
      : setup.captureCharFrame(),
  );
} finally {
  setup.renderer.destroy();
}
