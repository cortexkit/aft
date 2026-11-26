import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

// The bash abort path logs a line and flushes it because the host often exits
// right afterwards. The flush must not return before the line is on disk.
test("flushLog resolves only after buffered lines reach the log file", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-logger-flush-"));
  roots.push(root);
  const child = Bun.spawnSync({
    cmd: [
      process.execPath,
      "-e",
      `
      import { flushLog, getLogFilePath, log } from "./src/logger.ts";
      log("line written before exit");
      await flushLog();
      console.log(getLogFilePath());
      process.exit(0);
    `,
    ],
    env: { ...process.env, BUN_TEST: "1", TMPDIR: root, AFT_LOG_STDERR: "0" },
  });
  expect(child.exitCode).toBe(0);
  const logPath = child.stdout.toString().trim();
  expect(logPath.startsWith(root)).toBe(true);
  expect(readFileSync(logPath, "utf8")).toContain("line written before exit");
});
