/** @jsxImportSource @opentui/solid */
// Mounts the OpenCode 2 TUI's prompt footer claim with OpenTUI's test renderer
// and prints character frames. Run as its own process with the Solid transform
// preloaded (`bun --preload @opentui/solid/preload`): other test files replace
// solid-js and the OpenTUI JSX runtime with stubs for the whole test process,
// and a real render needs the real ones.
//
// The claim is drawn between two marker lines, "above" and "below", so a
// caller can tell a footer that renders nothing (the markers are adjacent)
// from one that renders an empty line.
//
// Input (environment): OPENCODE_TUI_PREFERENCES_FILE is the preferences file
// the footer reads. AFT_FOOTER_STEPS, when set, is a JSON array of edits
// applied one at a time after the first frame: a string is written as the
// file's text, and null deletes the file. After each edit another frame is
// printed once the footer has reacted (or a deadline passes). Frames are
// separated by a line holding FRAME_SEPARATOR. The footer stays mounted the
// whole time, so every change it shows came from the live file watch.

import { rm, writeFile } from "node:fs/promises";
import { testRender } from "@opentui/solid";
import { setupV2Tui } from "../../tui/v2.tsx";

const FRAME_SEPARATOR = "=====frame=====";
const SETTLE_MS = 500;
const UPDATE_DEADLINE_MS = 5_000;
const POLL_MS = 25;

const prefsFile = process.env.OPENCODE_TUI_PREFERENCES_FILE;
if (!prefsFile) throw new Error("OPENCODE_TUI_PREFERENCES_FILE must name the preferences file");
const steps = JSON.parse(process.env.AFT_FOOTER_STEPS ?? "[]") as Array<string | null>;

const status = {
  success: true,
  version: "0.62.0",
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
  status_bar: { errors: 0, warnings: 0, dead_code: 0, unused_exports: 0, duplicates: 1, todos: 0 },
  message: "",
};

type Claim = { append: string; render(input: { sessionID?: string }): unknown };
const claims: Claim[] = [];
const rpc = {
  async getStatus() {
    return status;
  },
  events: {
    on() {
      return () => {};
    },
  },
};
const context = {
  client: { rpc: () => rpc },
  keymap: { layer() {} },
  ui: {
    dialog: { async alert() {} },
    router: { current: () => ({ type: "session", sessionID: "ses_tui" }) },
    slot(claim: Claim) {
      claims.push(claim);
      return () => {};
    },
  },
};

const cleanup = await setupV2Tui(context as never);
const footer = claims.find((claim) => claim.append === "prompt.footer.status");
if (!footer) throw new Error("setupV2Tui claimed no prompt.footer.status slot");

const setup = await testRender(
  () => (
    <box flexDirection="column">
      <text>above</text>
      {footer.render({ sessionID: "ses_tui" }) as never}
      <text>below</text>
    </box>
  ),
  { width: 60, height: 6 },
);

async function frame(): Promise<string> {
  await setup.renderOnce();
  return setup.captureCharFrame();
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

try {
  // The preferences read and the status fetch are both asynchronous; give
  // them time to land before the first frame.
  await sleep(SETTLE_MS);
  let previous = await frame();
  process.stdout.write(previous);

  for (const step of steps) {
    if (step === null) await rm(prefsFile, { force: true });
    else await writeFile(prefsFile, step, "utf8");
    // The file watch debounces, so poll until the frame changes. A footer
    // that ignores the edit leaves the frame as it was and the deadline ends
    // the wait; the caller's assertion is what reports that.
    const deadline = Date.now() + UPDATE_DEADLINE_MS;
    let next = await frame();
    while (next === previous && Date.now() < deadline) {
      await sleep(POLL_MS);
      next = await frame();
    }
    // A footer that has just been shown fetches its status asynchronously.
    await sleep(SETTLE_MS);
    previous = await frame();
    process.stdout.write(`\n${FRAME_SEPARATOR}\n`);
    process.stdout.write(previous);
  }
} finally {
  cleanup();
  setup.renderer.destroy();
}
