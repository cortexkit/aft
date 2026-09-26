/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
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

function renderFrame(status: unknown): string[] {
  const result = spawnSync(
    process.execPath,
    [
      "--preload",
      "@opentui/solid/preload",
      join(__dirname, "fixtures", "render-sidebar-frame.tsx"),
    ],
    {
      cwd: join(__dirname, "..", ".."),
      encoding: "utf8",
      env: {
        ...process.env,
        AFT_SIDEBAR_SNAPSHOT: JSON.stringify(status),
        AFT_SIDEBAR_WIDTH: String(SIDEBAR_WIDTH),
      },
      timeout: 60_000,
    },
  );
  if (result.status !== 0) throw new Error(`sidebar render failed: ${result.stderr}`);
  return result.stdout.split("\n").map((line) => line.trimEnd());
}

describe("the sidebar at its real width", () => {
  test("a long status value wraps under the value column and never overwrites its label", () => {
    const frame = renderFrame(snapshot());
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
  });

  test("missing developer tools read as git features off, not DEGRADED, and keep the explanation", () => {
    const frame = renderFrame(snapshot());
    expect(frame[1]).toContain("AFT");
    expect(frame[1]).toContain("⚠ git features off");
    expect(frame.join("\n")).not.toContain("DEGRADED");
    expect(frame.join(" ")).toContain("xcode-select --install");
  });

  test("a real degradation still carries the DEGRADED badge", () => {
    const frame = renderFrame(snapshot({ degraded_reasons: ["home_root"] }));
    expect(frame[1]).toContain("DEGRADED");
  });
});
