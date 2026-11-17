import { spawnSync } from "@cortexkit/aft-bridge";
import { findAftBinary, missingAftBinaryMessage } from "../lib/binary-probe.js";
import { CLI } from "../lib/cli.js";

/** The native process owns the inventory, exact confirmation and deletion locks. */
export function runOldCacheCleanup(argv: string[]): number {
  const binary = findAftBinary();
  if (!binary) {
    console.error(missingAftBinaryMessage(`${CLI} cache clean-old`));
    return 1;
  }
  if (argv.some((arg) => !["--yes", "--help", "-h"].includes(arg))) {
    console.error("usage: aft cache clean-old [--yes]");
    return 2;
  }
  const result = spawnSync(binary, ["cache", "clean-old", ...argv], {
    stdio: "inherit",
    env: process.env,
  });
  if (result.error) console.error(`Old cache cleanup failed to start: ${result.error.message}`);
  return result.status ?? 1;
}
