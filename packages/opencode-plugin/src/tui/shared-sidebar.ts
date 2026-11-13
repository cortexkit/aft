import type * as SharedSidebar from "@cortexkit/opencode-sidebar";

// Raw-source hosts use their ordinary runtime; published hosts use the virtual
// runtime module selected by build-tui.ts instead of this loader.
const generatedModule = "../tui-compiled/shared-sidebar-local.tsx";
const sidebar: typeof SharedSidebar = await import(generatedModule).catch((cause: unknown) => {
  throw new Error(
    "AFT's generated shared sidebar could not be loaded. Run `bun run --cwd packages/opencode-plugin build:tui` before loading the TUI from source.",
    { cause },
  );
});

export const SidebarFrame = sidebar.SidebarFrame;
export type { SidebarTheme } from "@cortexkit/opencode-sidebar";
