/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * The OpenCode 2 prompt footer under the `footer` switch in the `aft` entry of
 * tui-preferences.jsonc, rendered by OpenTUI's own test renderer in a child
 * process (see fixtures/render-v2-footer.tsx). The fixture draws the footer
 * claim between the lines "above" and "below", so a hidden footer leaves them
 * adjacent and a shown one puts the health line between them.
 */

const FRAME_SEPARATOR = "=====frame=====";
const HEALTH_LINE = "AFT E0 W0 | D0 U0 C1 | T0";
const RENDER_PROCESS_TIMEOUT_MS = 30_000;
const TEST_TIMEOUT_MS = 45_000;

let dir: string;
let prefsFile: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "aft-tui-footer-test-"));
  prefsFile = join(dir, "tui-preferences.jsonc");
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

/**
 * Renders the footer, then applies each step to the preferences file in order
 * (a string is written as its text, null deletes it), returning the first
 * frame followed by one frame per step.
 */
async function renderFrames(...steps: Array<string | null>): Promise<string[][]> {
  const child = spawn(
    process.execPath,
    ["--preload", "@opentui/solid/preload", join(__dirname, "fixtures", "render-v2-footer.tsx")],
    {
      cwd: join(__dirname, "..", ".."),
      env: {
        ...process.env,
        OPENCODE_TUI_PREFERENCES_FILE: prefsFile,
        AFT_FOOTER_STEPS: JSON.stringify(steps),
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

  const output = await new Promise<string>((resolve, reject) => {
    const describeOutput = () =>
      `stderr: ${stderr.trim() || "(empty)"}; stdout: ${stdout.trim() || "(empty)"}`;
    const timeout = setTimeout(() => {
      child.kill("SIGKILL");
      reject(
        new Error(
          `footer render child timed out after ${RENDER_PROCESS_TIMEOUT_MS}ms; ${describeOutput()}`,
        ),
      );
    }, RENDER_PROCESS_TIMEOUT_MS);
    child.once("error", (error) => {
      clearTimeout(timeout);
      reject(new Error(`footer render child failed to start: ${error.message}`));
    });
    child.once("close", (code, signal) => {
      clearTimeout(timeout);
      if (code !== 0) {
        reject(
          new Error(
            `footer render child exited with code ${code ?? "null"}${signal ? ` and signal ${signal}` : ""}; ${describeOutput()}`,
          ),
        );
        return;
      }
      resolve(stdout);
    });
  });

  return output
    .split(`\n${FRAME_SEPARATOR}\n`)
    .map((frame) => frame.split("\n").map((line) => line.trim()));
}

/** The non-blank lines between the two markers; with `raw`, blank lines are kept too. */
function footerLines(frame: string[], raw = false): string[] {
  const above = frame.indexOf("above");
  const below = frame.indexOf("below");
  expect(above).toBeGreaterThan(-1);
  expect(below).toBeGreaterThan(above);
  const between = frame.slice(above + 1, below);
  return raw ? between : between.filter((line) => line.length > 0);
}

describe("the OpenCode 2 prompt footer switch", () => {
  test(
    "shows the health line by default, with no preferences file",
    async () => {
      const [frame] = await renderFrames();
      expect(footerLines(frame!)).toEqual([HEALTH_LINE]);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    'renders nothing with "footer": false, not even an empty line',
    async () => {
      await writeFile(prefsFile, `{ "aft": { "footer": false } }\n`, "utf8");
      const [frame] = await renderFrames();
      expect(frame!.join("\n")).not.toContain("AFT");
      expect(footerLines(frame!, true)).toEqual([]);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "an invalid value keeps the default and shows the line",
    async () => {
      await writeFile(prefsFile, `{ "aft": { "footer": "no" } }\n`, "utf8");
      const [frame] = await renderFrames();
      expect(footerLines(frame!)).toEqual([HEALTH_LINE]);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "turning the switch off while running hides the line without a restart",
    async () => {
      await writeFile(prefsFile, `{ "aft": { "footer": true } }\n`, "utf8");
      const [before, after] = await renderFrames(`{ "aft": { "footer": false } }\n`);
      expect(footerLines(before!)).toEqual([HEALTH_LINE]);
      expect(footerLines(after!, true)).toEqual([]);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "turning the switch back on while running shows the line again",
    async () => {
      await writeFile(prefsFile, `{ "aft": { "footer": false } }\n`, "utf8");
      const [before, after] = await renderFrames(`{ "aft": { "footer": true } }\n`);
      expect(footerLines(before!, true)).toEqual([]);
      expect(footerLines(after!)).toEqual([HEALTH_LINE]);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "deleting the preferences file while the line is off brings it back without a remount",
    async () => {
      await writeFile(prefsFile, `{ "aft": { "footer": false } }\n`, "utf8");
      const [before, after] = await renderFrames(null);
      expect(footerLines(before!, true)).toEqual([]);
      expect(footerLines(after!)).toEqual([HEALTH_LINE]);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "recreating the deleted file with the line off hides it again",
    async () => {
      await writeFile(prefsFile, `{ "aft": { "footer": false } }\n`, "utf8");
      const [before, deleted, recreated] = await renderFrames(
        null,
        `{ "aft": { "footer": false } }\n`,
      );
      expect(footerLines(before!, true)).toEqual([]);
      expect(footerLines(deleted!)).toEqual([HEALTH_LINE]);
      expect(footerLines(recreated!, true)).toEqual([]);
    },
    TEST_TIMEOUT_MS,
  );
});
