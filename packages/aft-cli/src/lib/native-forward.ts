import { constants } from "node:os";
import { spawnSync } from "@cortexkit/aft-bridge";
import { findAftBinary, missingAftBinaryMessage } from "./binary-probe.js";
import { CLI } from "./cli.js";
import { getSelfVersion } from "./self-version.js";

/** Prefer the running CLI's cache version; unknown versions retain newest-cache discovery. */
export function forwardToNative(command: "index" | "cache" | "backups", argv: string[]): number {
  const version = getSelfVersion();
  const preferred = /^v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version) ? version : undefined;
  const binary = findAftBinary(preferred);
  if (!binary) {
    console.error(missingAftBinaryMessage(`${CLI} ${command}`, preferred));
    return 1;
  }
  const result = spawnSync(binary, [command, ...argv], { stdio: "inherit", env: process.env });
  if (result.error) {
    console.error(`aft ${command} failed to start ${binary}: ${result.error.message}`);
    return 1;
  }
  if (result.status !== null) return result.status;
  const signal = result.signal ? constants.signals[result.signal] : undefined;
  return signal === undefined ? 1 : 128 + signal;
}
