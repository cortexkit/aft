import { expect, test } from "bun:test";
import { statSync } from "node:fs";
import { join } from "node:path";
import { cachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
import { createHarness } from "./e2e/helpers.js";

test("Pi e2e helper creates private HOME XDG and storage before bridge spawn", async () => {
  const binaryPath = cachedExecutable(`#!${process.execPath}
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const request = JSON.parse(buffer.slice(0, newline));
    buffer = buffer.slice(newline + 1);
    process.stdout.write(JSON.stringify({ id: request.id, success: true, env: process.env }) + "\\n");
  }
});
`);
  const harness = await createHarness({ binaryPath }, { noFixtures: true });
  try {
    const result = await harness.bridge.send("status");
    const env = result.env as Record<string, string>;
    for (const [key, directory] of Object.entries({
      HOME: "home",
      XDG_CONFIG_HOME: "config",
      XDG_DATA_HOME: "data",
      XDG_STATE_HOME: "state",
      XDG_CACHE_HOME: "cache",
      AFT_STORAGE_DIR: "storage",
    })) {
      expect(env[key]).toBe(join(harness.tempDir, ".aft-env", directory));
      expect(statSync(env[key]).isDirectory()).toBe(true);
    }
  } finally {
    await harness.cleanup();
  }
});
