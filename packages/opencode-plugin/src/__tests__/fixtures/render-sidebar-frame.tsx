/** @jsxImportSource @opentui/solid */
// Renders the shared AFT sidebar panel with OpenTUI's test renderer and prints
// the character frame. Run as its own process with the Solid transform
// preloaded (`bun --preload @opentui/solid/preload`): other test files replace
// solid-js and the OpenTUI JSX runtime with stubs for the whole test process,
// and a real render needs the real ones.
//
// Input (environment): AFT_SIDEBAR_SNAPSHOT is the status snapshot as JSON,
// AFT_SIDEBAR_WIDTH the panel width in columns.

import { testRender } from "@opentui/solid";
import { DEFAULT_PREFS } from "../../tui/preferences.ts";
import { AftSidebarPanel } from "../../tui/sidebar-view.tsx";

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
};

const setup = await testRender(
  () => (
    <AftSidebarPanel
      palette={palette}
      snapshot={snapshot}
      prefs={structuredClone(DEFAULT_PREFS)}
      collapsed={false}
      onToggleCollapsed={() => {}}
      pluginVersion="0.0.0"
    />
  ),
  { width, height: 80 },
);
await setup.renderOnce();
process.stdout.write(setup.captureCharFrame());
setup.renderer.destroy();
