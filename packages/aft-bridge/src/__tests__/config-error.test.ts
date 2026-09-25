import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  AftConfigError,
  CONFIG_ERROR_RESTART_NOTE,
  ConfigErrorTransportPool,
  configErrorStatusSnapshot,
  formatConfigErrorMessage,
  formatConfigErrorStatusLine,
} from "../config-error.js";
import { createAftTransportPool, subcConnectionFileError } from "../transport-factory.js";

describe("config error state helpers", () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "aft-config-error-"));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test("the message keeps the original error and fix, then says a restart is needed", () => {
    const detail = "subc.connection_file is set but missing. Remove subc.connection_file.";
    const message = formatConfigErrorMessage(detail);
    expect(message.startsWith(detail)).toBe(true);
    expect(message.endsWith(CONFIG_ERROR_RESTART_NOTE)).toBe(true);
    expect(message).toContain("restart");
  });

  test("the status line is a single line", () => {
    const line = formatConfigErrorStatusLine("first line\nsecond   line");
    expect(line).toBe("AFT config error: first line second line");
    expect(configErrorStatusSnapshot("broken\nconfig")).toEqual({
      success: true,
      status: "config_error",
      config_error: "broken\nconfig",
      message: "AFT config error: broken config",
    });
  });

  test("a missing subc connection file yields the factory's own error text", async () => {
    const missing = join(dir, "absent.json");
    const message = await subcConnectionFileError(missing);
    expect(message).toContain("no subc connection file exists there");
    expect(message).toContain("remove subc.connection_file");
    // The transport factory refuses with the same text subcConnectionFileError returns.
    await expect(
      createAftTransportPool({
        harness: "opencode",
        binaryPath: null,
        poolOptions: {} as never,
        configOverrides: {},
        subcConnectionFile: missing,
      }),
    ).rejects.toThrow(message ?? "unreachable");
  });

  test("an unset or present subc connection file is not an error", async () => {
    const present = join(dir, "subc-connection.json");
    writeFileSync(present, "{}");
    expect(await subcConnectionFileError(undefined)).toBeNull();
    expect(await subcConnectionFileError("   ")).toBeNull();
    expect(await subcConnectionFileError(present)).toBeNull();
  });

  test("the config error pool never hands out a bridge", async () => {
    const pool = new ConfigErrorTransportPool("config is broken");
    expect(() => pool.getBridge("/p")).toThrow(AftConfigError);
    expect(pool.getActiveBridgeForRoot("/p")).toBeNull();
    expect(pool.activeBridges()).toEqual([]);
    await expect(pool.toolCall("/p", {} as never, "read")).rejects.toThrow("config is broken");
    await expect(pool.replaceBinary("/bin/aft")).rejects.toThrow("config is broken");
    await pool.shutdown();
    await pool.closeSession("/p", "s");
    expect(pool.isShutdown()).toBe(false);
  });
});
