// resolveBashConfig (Pi) — v0.27.2 bash graduation contract.
//
// Mirrors packages/opencode-plugin/src/__tests__/resolve-bash-config.test.ts
// so behavior parity between the two harnesses is locked at the test level.
// If you change semantics here, change the OpenCode version too.

import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { resolveBashConfig, resolveProjectOverridesForConfigure } from "../config.js";

function cfg(overrides: Partial<AftConfig>): AftConfig {
  return overrides as AftConfig;
}

describe("resolveBashConfig (Pi)", () => {
  test("bash.env resolves and is forwarded to Rust", () => {
    const config = cfg({
      disabled_tools: [],
      bash: { env: { TZ: "UTC", AFT_TEST_KEY: "private-value" } },
    });
    expect(resolveBashConfig(config).env).toEqual({ TZ: "UTC", AFT_TEST_KEY: "private-value" });
    expect(resolveProjectOverridesForConfigure(config).bash).toEqual({
      env: { TZ: "UTC", AFT_TEST_KEY: "private-value" },
    });
  });
  // ---- Top-level boolean shapes ----------------------------------------

  test("bash: true → fully enabled regardless of surface", () => {
    const r = resolveBashConfig(cfg({ bash: true }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: true,
      background: true,
    });
  });

  test("bash: false → fully disabled regardless of surface", () => {
    const r = resolveBashConfig(cfg({ bash: false }));
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

  test("PowerShell registration fallback defaults off and requires explicit opt-in", () => {
    expect(resolveBashConfig(cfg({ bash: true })).powershell_tool).toBe(false);
    expect(resolveBashConfig(cfg({ bash: { powershell_tool: true } })).powershell_tool).toBe(true);
  });

  test("host fallback defaults off unless explicitly enabled", () => {
    expect(resolveBashConfig(cfg({ bash: true })).host_fallback).toBe(false);
    expect(resolveBashConfig(cfg({ bash: {} })).host_fallback).toBe(false);
    expect(resolveBashConfig(cfg({})).host_fallback).toBe(false);
    expect(resolveBashConfig(cfg({ bash: { host_fallback: true } })).host_fallback).toBe(true);
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

  // ---- Defaults and the runtime gate ------------------------------------

  test("no top-level bash → all on", () => {
    const r = resolveBashConfig(cfg({}));
    expect(r.enabled).toBe(true);
    expect(r.rewrite).toBe(true);
  });

  test("bash.enabled:false turns only the runtime gate off", () => {
    const r = resolveBashConfig(cfg({ bash: { enabled: false } }));
    expect(r).toMatchObject({ enabled: false, rewrite: true, compress: true, background: true });
  });

  test("detach_on_user_message defaults to true and supports opt-out", () => {
    expect(resolveBashConfig(cfg({})).detach_on_user_message).toBe(true);
    expect(
      resolveBashConfig(cfg({ bash: { detach_on_user_message: false } })).detach_on_user_message,
    ).toBe(false);
  });

  test("watch_sync_max_ms defaults to 120 seconds, clamps, and accepts the old cap", () => {
    expect(resolveBashConfig(cfg({})).watch_sync_max_ms).toBe(120_000);
    expect(resolveBashConfig(cfg({ bash: { watch_sync_max_ms: 5 } })).watch_sync_max_ms).toBe(
      1_000,
    );
    expect(
      resolveBashConfig(cfg({ bash: { watch_sync_max_ms: 1_800_000 } })).watch_sync_max_ms,
    ).toBe(1_800_000);
  });

  test("worker_wait_max_ms defaults to 30 minutes and takes a configured value", () => {
    expect(resolveBashConfig(cfg({})).worker_wait_max_ms).toBe(1_800_000);
    expect(resolveBashConfig(cfg({ bash: true })).worker_wait_max_ms).toBe(1_800_000);
    expect(
      resolveBashConfig(cfg({ bash: { worker_wait_max_ms: 5_400_000 } })).worker_wait_max_ms,
    ).toBe(5_400_000);
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
});
