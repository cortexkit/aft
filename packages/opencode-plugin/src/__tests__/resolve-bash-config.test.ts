// resolveBashConfig — v0.27.2 bash graduation contract.
//
// Locks the resolution of the top-level `bash` surface (boolean OR
// object), plus the all-on default that fills in when it is not set. The
// retired `experimental.bash` block is refused or rewritten before this
// resolver sees a config.

import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { resolveBashConfig, resolveProjectOverridesForConfigure } from "../config.js";

function cfg(overrides: Partial<AftConfig>): AftConfig {
  return overrides as AftConfig;
}

describe("resolveBashConfig", () => {
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

  test("bash: true → fully enabled", () => {
    const r = resolveBashConfig(cfg({ bash: true }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: true,
      background: true,
    });
  });

  test("bash: false → runtime gate and sub-features off", () => {
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

  test("host fallback defaults off even when bash features default on", () => {
    expect(resolveBashConfig(cfg({ bash: true })).host_fallback).toBe(false);
    expect(resolveBashConfig(cfg({ bash: {} })).host_fallback).toBe(false);
    expect(resolveBashConfig(cfg({})).host_fallback).toBe(false);
  });

  test("bash.host_fallback is an explicit opt-in", () => {
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

  test("bash: { compress: false, background: false } → enabled; only rewrite default on", () => {
    const r = resolveBashConfig(cfg({ bash: { compress: false, background: false } }));
    expect(r).toMatchObject({
      enabled: true,
      rewrite: true,
      compress: false,
      background: false,
    });
  });

  // ---- Defaults and the runtime gate ------------------------------------

  test("bash.enabled:false turns the runtime gate off and keeps sub-feature defaults", () => {
    const r = resolveBashConfig(cfg({ bash: { enabled: false } }));
    expect(r).toMatchObject({ enabled: false, rewrite: true, compress: true, background: true });
    expect(resolveBashConfig(cfg({ bash: false })).enabled).toBe(false);
    expect(resolveBashConfig(cfg({ bash: true })).enabled).toBe(true);
    expect(resolveBashConfig(cfg({ bash: {} })).enabled).toBe(true);
  });

  test("no top-level bash → all on", () => {
    const r = resolveBashConfig(cfg({}));
    expect(r.enabled).toBe(true);
    expect(r.rewrite).toBe(true);
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
