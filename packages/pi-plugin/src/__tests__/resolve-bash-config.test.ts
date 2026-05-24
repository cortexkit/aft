// resolveBashConfig (Pi) — v0.27.2 bash graduation contract.
//
// Mirrors packages/opencode-plugin/src/__tests__/resolve-bash-config.test.ts
// so behavior parity between the two harnesses is locked at the test level.
// If you change semantics here, change the OpenCode version too.

import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { resolveBashConfig } from "../config.js";

function cfg(overrides: Partial<AftConfig>): AftConfig {
  return overrides as AftConfig;
}

describe("resolveBashConfig (Pi)", () => {
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

  test("legacy experimental.bash.rewrite=true → opt-in semantics (rewrite on, others off)", () => {
    const r = resolveBashConfig(cfg({ experimental: { bash: { rewrite: true } } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: false,
      background: false,
    });
  });

  test("legacy experimental.bash with all explicit false → DISABLED (no surface promotion)", () => {
    const r = resolveBashConfig(
      cfg({
        tool_surface: "recommended",
        experimental: { bash: { rewrite: false, compress: false, background: false } },
      }),
    );
    expect(r.enabled).toBe(false);
  });

  test("empty legacy experimental.bash: {} → falls through to surface default", () => {
    const r = resolveBashConfig(cfg({ tool_surface: "recommended", experimental: { bash: {} } }));
    expect(r.enabled).toBe(true);
  });

  // ---- Surface defaults ------------------------------------------------

  test("no top-level, no legacy, tool_surface=recommended → all on", () => {
    const r = resolveBashConfig(cfg({ tool_surface: "recommended" }));
    expect(r.enabled).toBe(true);
    expect(r.rewrite).toBe(true);
  });

  test("no top-level, no legacy, tool_surface=minimal → all off", () => {
    const r = resolveBashConfig(cfg({ tool_surface: "minimal" }));
    expect(r.enabled).toBe(false);
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
