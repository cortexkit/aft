// Runs the sidebar's preference state (createSidebarPreferences) in a Solid
// root and prints it as JSON, one line per state. Run as its own process with
// the Solid transform preloaded (`bun --preload @opentui/solid/preload`):
// other test files replace solid-js with stubs for the whole test process, and
// sidebar-view.tsx needs the real one.
//
// Input (environment): OPENCODE_TUI_PREFERENCES_FILE is the preferences file.
// AFT_SIDEBAR_PREFS_STEPS is a JSON array of edits applied one at a time
// after the first state: a string is written as the file's text, null deletes
// the file, and "<directory>" replaces the file with a directory of the same
// name, which is present at the path but cannot be read as a file. After each
// edit another line is printed once the state has changed or a deadline has
// passed.

import { mkdir, rm, writeFile } from "node:fs/promises";
import { createRoot } from "solid-js";
import { createSidebarPreferences } from "../../tui/sidebar-view.tsx";

const SETTLE_MS = 500;
const UPDATE_DEADLINE_MS = 2_000;
const POLL_MS = 25;

const prefsFile = process.env.OPENCODE_TUI_PREFERENCES_FILE;
if (!prefsFile) throw new Error("OPENCODE_TUI_PREFERENCES_FILE must name the preferences file");
const steps = JSON.parse(process.env.AFT_SIDEBAR_PREFS_STEPS ?? "[]") as Array<string | null>;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

await createRoot(async (dispose) => {
  const preferences = createSidebarPreferences();
  const state = () =>
    JSON.stringify({ prefs: preferences.prefs(), collapsed: preferences.collapsed() });
  try {
    await sleep(SETTLE_MS);
    let previous = state();
    process.stdout.write(`${previous}\n`);
    for (const step of steps) {
      if (step === null) {
        await rm(prefsFile, { force: true });
      } else if (step === "<directory>") {
        await rm(prefsFile, { force: true });
        await mkdir(prefsFile);
      } else {
        await writeFile(prefsFile, step, "utf8");
      }
      // Poll until the state changes; a state that never changes ends the
      // wait at the deadline, and the caller's assertion reports it.
      const deadline = Date.now() + UPDATE_DEADLINE_MS;
      while (state() === previous && Date.now() < deadline) await sleep(POLL_MS);
      previous = state();
      process.stdout.write(`${previous}\n`);
    }
  } finally {
    dispose();
  }
});
