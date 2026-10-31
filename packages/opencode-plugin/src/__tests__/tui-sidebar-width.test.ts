/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { join } from "node:path";

/**
 * The sidebar rendered by OpenTUI's own test renderer at the width the setup
 * drill measured: about 38 columns for the sidebar in a 140-column terminal.
 * Both OpenCode hosts draw this same panel (sidebar.tsx for OpenCode 1, v2.tsx
 * for OpenCode 2), so one frame covers both.
 */
const SIDEBAR_WIDTH = 38;
const GIT_OFF_REASON =
  "git features are off: macOS developer tools are not installed, and running /usr/bin/git would open Apple's install dialog. Install them with `xcode-select --install`, or put another git (for example Homebrew's) on PATH, then restart AFT.";

function snapshot(overrides: Record<string, unknown> = {}) {
  return {
    version: "0.58.0",
    project_root: "/work/project",
    canonical_root: "/work/project",
    cache_role: "main",
    degraded: true,
    degraded_reasons: [GIT_OFF_REASON],
    features: {},
    search_index: { status: "ready", files: 12, trigrams: 34 },
    // The daemon's missing-runtime wording, which the sidebar turns into its
    // longest status value.
    semantic_index: {
      status: "unavailable",
      refreshing_count: 0,
      error: "ONNX Runtime not found. Install it.",
    },
    disk: { trigram_disk_bytes: 1000, semantic_disk_bytes: 0 },
    ...overrides,
  };
}

const RENDER_PROCESS_TIMEOUT_MS = 30_000;
const TEST_TIMEOUT_MS = 45_000;

async function renderFrame(
  status: unknown,
  extraEnv: Record<string, string> = {},
): Promise<string[]> {
  const child = spawn(
    process.execPath,
    [
      "--preload",
      "@opentui/solid/preload",
      join(__dirname, "fixtures", "render-sidebar-frame.tsx"),
    ],
    {
      cwd: join(__dirname, "..", ".."),
      env: {
        ...process.env,
        AFT_SIDEBAR_SNAPSHOT: JSON.stringify(status),
        AFT_SIDEBAR_WIDTH: String(SIDEBAR_WIDTH),
        ...extraEnv,
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let stdout = "";
  let stderr = "";
  // Decode as a stream so a multi-byte character split across pipe chunks
  // (the frame contains "⚠" and "—") is never garbled.
  child.stdout?.setEncoding("utf8");
  child.stderr?.setEncoding("utf8");
  child.stdout?.on("data", (chunk: string) => {
    stdout += chunk;
  });
  child.stderr?.on("data", (chunk: string) => {
    stderr += chunk;
  });

  return await new Promise((resolve, reject) => {
    let settled = false;
    const finish = (complete: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      complete();
    };
    const timeout = setTimeout(() => {
      child.kill("SIGKILL");
      finish(() =>
        reject(
          new Error(
            `sidebar render child timed out after ${RENDER_PROCESS_TIMEOUT_MS}ms waiting for the OpenTUI render fixture to exit; stderr: ${stderr.trim() || "(empty)"}; stdout: ${stdout.trim() || "(empty)"}`,
          ),
        ),
      );
    }, RENDER_PROCESS_TIMEOUT_MS);

    child.once("error", (error) => {
      finish(() =>
        reject(
          new Error(
            `sidebar render child failed to start: ${error.message}; stderr: ${stderr.trim() || "(empty)"}; stdout: ${stdout.trim() || "(empty)"}`,
          ),
        ),
      );
    });
    child.once("close", (code, signal) => {
      finish(() => {
        if (code !== 0) {
          reject(
            new Error(
              `sidebar render child exited with code ${code ?? "null"}${signal ? ` and signal ${signal}` : ""}; stderr: ${stderr.trim() || "(empty)"}; stdout: ${stdout.trim() || "(empty)"}`,
            ),
          );
          return;
        }
        resolve(stdout.split("\n").map((line) => line.trimEnd()));
      });
    });
  });
}

describe("the sidebar at its real width", () => {
  test(
    "a long status value wraps under the value column and never overwrites its label",
    async () => {
      const frame = await renderFrame(snapshot());
      const semantic = frame.findIndex((line) => line.trim() === "Semantic Index");
      expect(semantic).toBeGreaterThan(-1);
      const status = frame[semantic + 1] ?? "";
      expect(status).toMatch(/^ Status unavailable — ONNX Runtime/);
      // The continuation lines start in the value column, under "unavailable".
      const valueColumn = status.indexOf("unavailable");
      const continuation = frame[semantic + 2] ?? "";
      expect(continuation.search(/\S/)).toBe(valueColumn);
      expect(frame.slice(semantic + 1, semantic + 4).join(" ")).toContain("doctor --fix");
      for (const line of frame) expect(line.length).toBeLessThanOrEqual(SIDEBAR_WIDTH);
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "missing developer tools read as git features off, not DEGRADED, and keep the explanation",
    async () => {
      const frame = await renderFrame(snapshot());
      expect(frame[1]).toContain("AFT");
      expect(frame[1]).toContain("⚠ git features off");
      expect(frame.join("\n")).not.toContain("DEGRADED");
      expect(frame.join(" ")).toContain("xcode-select --install");
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "a real degradation still carries the DEGRADED badge",
    async () => {
      const frame = await renderFrame(snapshot({ degraded_reasons: ["home_root"] }));
      expect(frame[1]).toContain("DEGRADED");
    },
    TEST_TIMEOUT_MS,
  );

  test(
    "a render slower than the default 5 s test limit still yields the complete frame",
    async () => {
      // The child waits 6 s before its first frame, longer than Bun's default
      // per-test limit, so this proves the helper waits for the child to exit
      // and returns the whole frame rather than an early or empty read.
      const frame = await renderFrame(snapshot(), { AFT_SIDEBAR_TEST_RENDER_DELAY_MS: "6000" });
      expect(frame[1]).toContain("AFT");
      expect(frame.findIndex((line) => line.trim() === "Semantic Index")).toBeGreaterThan(-1);
    },
    TEST_TIMEOUT_MS,
  );
});

describe("operator coordination lines in the visible sidebar", () => {
  for (const line of [
    "Waiting for another AFT process that is embedding the same new code; results will follow.",
    "Can't coordinate embedding with other AFT processes: permission denied. Retrying in 1m 20s.",
  ]) {
    test(
      `wraps the approved line without losing text: ${line}`,
      async () => {
        const frame = await renderFrame(
          snapshot({
            degraded: false,
            degraded_reasons: [],
            semantic_index: { status: "ready", refreshing_count: 0, pending_reason: line },
          }),
        );
        expect(frame.join(" ").replace(/\s+/g, " ")).toContain(line);
        for (const row of frame) expect(row.length).toBeLessThanOrEqual(SIDEBAR_WIDTH);
      },
      TEST_TIMEOUT_MS,
    );
  }
});
