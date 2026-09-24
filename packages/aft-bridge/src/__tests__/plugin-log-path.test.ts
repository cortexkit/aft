import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { isTestEnvironment, resolvePluginLogPath } from "../storage-paths.js";

function lookup(env: Record<string, string>) {
  return (name: string) => env[name];
}

describe("resolvePluginLogPath", () => {
  test("a test run logs under the temp directory, never the storage root", () => {
    const path = resolvePluginLogPath({
      lookup: lookup({ BUN_TEST: "1", HOME: "/home/dev", TMPDIR: "/tmp/run" }),
      platform: "other",
    });
    expect(path).toBe(join("/tmp/run", "aft-plugin-test.log"));
    expect(path.startsWith(join("/home/dev", ".local", "share", "cortexkit"))).toBe(false);
  });

  test("NODE_ENV=test counts as a test run", () => {
    const context = {
      lookup: lookup({ NODE_ENV: "test", TMPDIR: "/tmp/run" }),
      platform: "other" as const,
    };
    expect(isTestEnvironment(context)).toBe(true);
    expect(resolvePluginLogPath(context)).toBe(join("/tmp/run", "aft-plugin-test.log"));
  });

  test("outside tests the plugin logs to aft-plugin.log in the storage root", () => {
    const path = resolvePluginLogPath({
      lookup: lookup({ HOME: "/home/dev", TMPDIR: "/tmp/run" }),
      platform: "other",
      currentDirectory: "/",
    });
    expect(path).toBe(
      join("/home/dev", ".local", "share", "cortexkit", "aft", "logs", "aft-plugin.log"),
    );
  });
});
