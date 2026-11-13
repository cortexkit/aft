/// <reference path="../bun-test.d.ts" />

import { beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";

const pluginRoot = join(__dirname, "..", "..");
beforeAll(() => {
  if (existsSync(join(pluginRoot, "src/tui-compiled/shared-sidebar-local.tsx"))) return;
  const built = spawnSync(process.execPath, ["run", "build:tui"], {
    cwd: pluginRoot,
    encoding: "utf8",
  });
  if (built.status !== 0) throw new Error(`build:tui failed: ${built.stderr}`);
});

// OpenCode loads the plugin's `./tui` export, which now points at entry.mjs.
// In a development checkout there is no host virtual runtime registry, so the
// loader must fall back to the raw TSX module. Published installs are different:
// OpenTUI skips the Solid transform for code loaded from node_modules, so the
// packed plugin must also ship a precompiled TUI tree. That published-path
// verification lives in scripts/smoke-tui-pack-install.ts; this test covers the
// repo-checkout fallback path and the runtime dependencies it still needs.
describe("tui entry module resolution", () => {
  test("the ./tui entry falls back to the raw TSX module in a repo checkout", async () => {
    const raw = (await import("../tui/index.tsx")) as {
      default?: { id?: string; tui?: unknown };
    };
    const entry = (await import("../tui/entry.mjs")) as {
      default?: { id?: string; tui?: unknown };
    };

    expect(raw.default).toBeDefined();
    expect(entry.default).toBe(raw.default);
    expect(entry.default?.id).toBe("aft-opencode");
    expect(typeof entry.default?.tui).toBe("function");
  });

  test("the raw TSX entry still imports without a missing-module error", async () => {
    const mod = (await import("../tui/index.tsx")) as {
      default?: { id?: string; tui?: unknown };
    };
    expect(mod.default).toBeDefined();
    expect(mod.default?.id).toBe("aft-opencode");
    expect(typeof mod.default?.tui).toBe("function");
  });

  test("the @opentui/solid jsx runtime resolves from this package", () => {
    // The pragma compiles to `@opentui/solid/jsx-dev-runtime`; if it is not a
    // declared dep, this throws MODULE_NOT_FOUND.
    const resolved = require.resolve("@opentui/solid/jsx-dev-runtime");
    expect(resolved).toContain("@opentui");
    // solid-js must resolve to a single physical copy (dual-instance would break
    // Solid's reactive context across the OpenTUI tree even though it resolves).
    const solid = require.resolve("solid-js");
    expect(solid).toContain("solid-js");
  });

  test("opentui and solid are exact-pinned runtime dependencies", async () => {
    // Published TUI entries import these from inside OpenCode's plugin cache
    // under node_modules, where the host's Solid transform does not apply.
    // They must ship as runtime deps, and exact pins keep the local copy on the
    // host's embedded OpenTUI line instead of drifting ahead on a range.
    const pkg = (await import("../../package.json")) as {
      default: { dependencies?: Record<string, string> };
    };
    const deps = pkg.default.dependencies ?? {};
    for (const required of ["@opentui/core", "@opentui/solid", "solid-js"]) {
      const pin = deps[required];
      expect(pin).toBeDefined();
      expect(/^\d/.test(pin ?? "")).toBe(true);
    }
  });

  test("the bare Bun raw fallback renders the shared frame through both TUI entries", () => {
    const rendered = spawnSync(
      process.execPath,
      ["src/__tests__/fixtures/render-sidebar-frame.tsx"],
      {
        cwd: pluginRoot,
        encoding: "utf8",
        timeout: 30_000,
        env: {
          ...process.env,
          AFT_SIDEBAR_ENTRY_CHECK: "true",
          AFT_SIDEBAR_SNAPSHOT: JSON.stringify({
            version: "0.64.0",
            cache_role: "main",
            degraded: true,
            degraded_reasons: ["home_root"],
            search_index: { status: "ready", files: 12 },
            semantic_index: { status: "ready" },
          }),
        },
      },
    );
    expect(rendered.stderr).toBe("");
    expect(rendered.status).toBe(0);
    expect(rendered.stdout).toContain("▼ AFT   DEGRADED");
    expect(rendered.stdout).toContain("v0.64.0");
    expect(rendered.stdout).toContain("Search Index");
    expect(rendered.stdout).toContain("ready");
  }, 45_000);
});
