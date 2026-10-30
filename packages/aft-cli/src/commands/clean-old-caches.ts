import { spawnSync } from "@cortexkit/aft-bridge";
import { findAftBinary, missingAftBinaryMessage } from "../lib/binary-probe.js";
import { CLI } from "../lib/cli.js";

/** The native process owns the inventory, exact confirmation and deletion locks. */
export function runOldCacheCleanup(argv: string[]): number {
  const binary = findAftBinary();
  if (!binary) {
    console.error(missingAftBinaryMessage(`${CLI} doctor --clean-old-caches`));
    return 1;
  }
  if (argv.some((arg) => !["--clean-old-caches", "--yes", "--help", "-h"].includes(arg))) {
    console.error("usage: aft doctor --clean-old-caches [--yes]");
    return 2;
  }
  const result = spawnSync(
    binary,
    ["clean-old-caches", ...argv.filter((arg) => arg !== "--clean-old-caches")],
    {
      stdio: "inherit",
      env: process.env,
    },
  );
  if (result.error) console.error(`Old cache cleanup failed to start: ${result.error.message}`);
  return result.status ?? 1;
}
