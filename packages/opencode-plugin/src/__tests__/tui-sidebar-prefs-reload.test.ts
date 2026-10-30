/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEFAULT_PREFS } from "../tui/preferences";

/**
 * The sidebar's live preference state (createSidebarPreferences) as the
 * preferences file is deleted or made unreadable, run with the real Solid
 * runtime in a child process (see fixtures/sidebar-prefs-steps.ts).
 */

const RENDER_PROCESS_TIMEOUT_MS = 30_000;
const TEST_TIMEOUT_MS = 45_000;
const CUSTOM = `{ "aft": { "footer": false, "collapsed": true, "header": { "label": "TOOLS" }, "sections": { "compression": false } } }\n`;

type SidebarState = { prefs: typeof DEFAULT_PREFS; collapsed: boolean };

let dir: string;
let prefsFile: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "aft-tui-sidebar-prefs-test-"));
  prefsFile = join(dir, "tui-preferences.jsonc");
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

/** The first state, then one state per step (see the fixture for step forms). */
async function runSteps(...steps: Array<string | null>): Promise<SidebarState[]> {
  const child = spawn(
    process.execPath,
    ["--preload", "@opentui/solid/preload", join(__dirname, "fixtures", "sidebar-prefs-steps.ts")],
    {
      cwd: join(__dirname, "..", ".."),
      env: {
        ...process.env,
        OPENCODE_TUI_PREFERENCES_FILE: prefsFile,
        AFT_SIDEBAR_PREFS_STEPS: JSON.stringify(steps),
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let stdout = "";
  let stderr = "";
  child.stdout?.setEncoding("utf8");
  child.stderr?.setEncoding("utf8");
  child.stdout?.on("data", (chunk: string) => {
    stdout += chunk;
  });
  child.stderr?.on("data", (chunk: string) => {
    stderr += chunk;
  });
  await new Promise<void>((resolve, reject) => {
    const describeOutput = () =>
      `stderr: ${stderr.trim() || "(empty)"}; stdout: ${stdout.trim() || "(empty)"}`;
    const timeout = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`sidebar preferences child timed out; ${describeOutput()}`));
    }, RENDER_PROCESS_TIMEOUT_MS);
    child.once("error", (error) => {
      clearTimeout(timeout);
      reject(new Error(`sidebar preferences child failed to start: ${error.message}`));
    });
    child.once("close", (code) => {
      clearTimeout(timeout);
      if (code === 0) resolve();
      else reject(new Error(`sidebar preferences child exited with ${code}; ${describeOutput()}`));
    });
  });
  return stdout
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line) as SidebarState);
}

describe("the sidebar's live preferences", () => {
  test(
    "return to the defaults when the preferences file is deleted",
    async () => {
      await writeFile(prefsFile, CUSTOM, "utf8");
      const [before, deleted] = await runSteps(null);
      expect(before!.prefs.header.label).toBe("TOOLS");
      expect(before!.prefs.sections.compression).toBe(false);
      expect(before!.collapsed).toBe(true);
      expect(deleted).toEqual({ prefs: DEFAULT_PREFS, collapsed: false });
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "keep their values when the file is present but unreadable",
    async () => {
      await writeFile(prefsFile, CUSTOM, "utf8");
      const [before, unreadable] = await runSteps("<directory>");
      expect(before!.prefs.header.label).toBe("TOOLS");
      expect(unreadable).toEqual(before);
    },
    TEST_TIMEOUT_MS,
  );
});
