import { createHash } from "node:crypto";
import { join } from "node:path";

/**
 * Compute a stable hash for a project directory.
 * Used to scope RPC port files per-project so multiple
 * OpenCode Desktop instances don't overwrite each other.
 */
export function projectHash(directory: string): string {
  // Normalize: strip trailing slashes
  const normalized = directory.replace(/\/+$/, "");
  return createHash("sha256").update(normalized).digest("hex").slice(0, 16);
}

/**
 * Legacy per-project RPC port file path (single file).
 *
 * Kept exported for backward-compatibility readers — when an older plugin
 * instance is running alongside a newer one, the older one still writes
 * to this path. New code prefers `rpcPortFileDir` (one file per instance)
 * so that two plugin instances under `opencode --port 0` don't overwrite
 * each other's port info. The client falls back to the legacy file if
 * the new directory has no entries.
 */
export function rpcPortFilePath(storageDir: string, directory: string): string {
  const hash = projectHash(directory);
  return join(storageDir, "rpc", hash, "port");
}

/**
 * Per-project RPC port directory. Each plugin instance writes a file
 * `<instance-id>.json` into this directory so the client can discover
 * ALL active plugin instances (e.g. the two created by OpenCode TUI when
 * launched with `--port 0`). The client tries each port and uses the
 * first one whose bridge is warm.
 *
 * Replaces the single `port` file used pre-v0.28.2 (which suffered from
 * last-write-wins racing under `--port 0`).
 */
export function rpcPortFileDir(storageDir: string, directory: string): string {
  const hash = projectHash(directory);
  return join(storageDir, "rpc", hash, "ports");
}
