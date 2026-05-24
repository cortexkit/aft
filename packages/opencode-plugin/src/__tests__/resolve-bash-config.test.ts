// resolveBashConfig — v0.27.2 bash graduation contract.
//
// Locks the precedence rules between the new top-level `bash` surface
// (boolean OR object) and the legacy `experimental.bash.*` block, plus
// the `tool_surface` surface-default that fills in when neither is set.

import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { resolveBashConfig } from "../config.js";

function cfg(overrides: Partial<AftConfig>): AftConfig {
  return overrides as AftConfig;
}

describe("resolveBashConfig", () => {
  // ---- Top-level boolean shapes ----------------------------------------

  test("bash: true → fully enabled regardless of surface", () => {
    const r = resolveBashConfig(cfg({ bash: true, tool_surface: "minimal" }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: true,
      background: true,
    });
  });

  test("bash: false → fully disabled regardless of surface", () => {
    const r = resolveBashConfig(cfg({ bash: false, tool_surface: "all" }));
    expect(r).toMatchObject({
      enabled: false,
      rewrite: false,
      compress: false,
      background: false,
    });
  });

  // ---- Top-level object form (missing sub-keys default true) -----------

  test("bash: {} → enabled with all sub-features on (graduated defaults)", () => {
    const r = resolveBashConfig(cfg({ bash: {} }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: true,
      background: true,
    });
  });

  test("bash: { rewrite: false } → enabled; rewrite off, others default on", () => {
    const r = resolveBashConfig(cfg({ bash: { rewrite: false } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: false,
      compress: true,
      background: true,
    });
  });

  test("bash: { compress: false, background: false } → enabled; only rewrite default on", () => {
    const r = resolveBashConfig(cfg({ bash: { compress: false, background: false } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: false,
      background: false,
    });
  });

  // ---- Top-level wins over legacy --------------------------------------

  test("top-level bash: true wins over legacy experimental.bash.rewrite: false", () => {
    const r = resolveBashConfig(cfg({ bash: true, experimental: { bash: { rewrite: false } } }));
    expect(r).toMatchObject({ enabled: true, rewrite: true, compress: true, background: true });
  });

  test("top-level bash: false wins over legacy experimental.bash.background: true", () => {
    const r = resolveBashConfig(cfg({ bash: false, experimental: { bash: { background: true } } }));
    expect(r.enabled).toBe(false);
    expect(r.background).toBe(false);
  });

  // ---- Legacy fallback (no top-level) ----------------------------------

  test("legacy experimental.bash.rewrite=true only → enabled; rewrite on, others off (opt-in semantics preserved)", () => {
    const r = resolveBashConfig(cfg({ experimental: { bash: { rewrite: true } } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: false,
      background: false,
    });
  });

  test("legacy experimental.bash.compress=true only → enabled; compress on, others off", () => {
    const r = resolveBashConfig(cfg({ experimental: { bash: { compress: true } } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: false,
      compress: true,
      background: false,
    });
  });

  test("legacy experimental.bash.background=true only → enabled; only background on", () => {
    const r = resolveBashConfig(cfg({ experimental: { bash: { background: true } } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: false,
      compress: false,
      background: true,
    });
  });

  test("legacy experimental.bash with all explicit false → DISABLED (no implicit surface promotion)", () => {
    // Important: when the user explicitly set every sub-flag to false in
    // the legacy block, they meant disabled — even on `recommended` surface.
    // Otherwise this surface-default fallback would silently override their
    // explicit opt-out.
    const r = resolveBashConfig(
      cfg({
        tool_surface: "recommended",
        experimental: { bash: { rewrite: false, compress: false, background: false } },
      }),
    );
    expect(r.enabled).toBe(false);
  });

  test("empty legacy experimental.bash: {} → falls through to surface default (no explicit opt-in)", () => {
    // An empty experimental block has no feature flag at all, so we don't
    // treat it as an explicit opt-out. Surface default kicks in instead.
    const r = resolveBashConfig(cfg({ tool_surface: "recommended", experimental: { bash: {} } }));
    expect(r.enabled).toBe(true);
  });

  // ---- Surface defaults (when nothing specified) -----------------------

  test("no top-level, no legacy, tool_surface=recommended → all on", () => {
    const r = resolveBashConfig(cfg({ tool_surface: "recommended" }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: true,
      background: true,
    });
  });

  test("no top-level, no legacy, tool_surface=all → all on", () => {
    const r = resolveBashConfig(cfg({ tool_surface: "all" }));
    expect(r.enabled).toBe(true);
  });

  test("no top-level, no legacy, tool_surface=minimal → all off", () => {
    const r = resolveBashConfig(cfg({ tool_surface: "minimal" }));
    expect(r).toMatchObject({
      enabled: false,
      rewrite: false,
      compress: false,
      background: false,
    });
  });

  test("no top-level, no legacy, no tool_surface → defaults to recommended (all on)", () => {
    const r = resolveBashConfig(cfg({}));
    expect(r.enabled).toBe(true);
    expect(r.rewrite).toBe(true);
  });

  // ---- Reminder tuning carries through ---------------------------------

  test("long_running_reminder_* on top-level carries through", () => {
    const r = resolveBashConfig(
      cfg({
        bash: { long_running_reminder_enabled: false, long_running_reminder_interval_ms: 5000 },
      }),
    );
    expect(r.long_running_reminder_enabled).toBe(false);
    expect(r.long_running_reminder_interval_ms).toBe(5000);
  });

  test("long_running_reminder_* on legacy carries through when top-level absent", () => {
    const r = resolveBashConfig(
      cfg({
        experimental: {
          bash: { rewrite: true, long_running_reminder_interval_ms: 2500 },
        },
      }),
    );
    expect(r.long_running_reminder_interval_ms).toBe(2500);
  });

  test("top-level reminder wins over legacy reminder when both set", () => {
    const r = resolveBashConfig(
      cfg({
        bash: { long_running_reminder_interval_ms: 1000 },
        experimental: { bash: { long_running_reminder_interval_ms: 2000 } },
      }),
    );
    expect(r.long_running_reminder_interval_ms).toBe(1000);
  });
});
